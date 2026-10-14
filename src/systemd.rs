//! The opt-in `systemd` mode: one user unit per agent.
//!
//! By default an agent is a child of the terminal UI: close the terminal and the
//! agent stops, because the tool stops it on the way out. That is the right default,
//! but it ties long work to a window staying open. This module is the other way, and
//! it is **opt-in per agent**: the tool writes a unit file under the user's own
//! systemd directory and asks the user's own `systemctl --user` to start it. The unit
//! belongs to the user, never to the system: no root, no system unit, no daemon.
//!
//! Four rules hold here.
//!
//! - **The same command the UI would run.** A unit's `ExecStart` is exactly
//!   `<this program> --run-agent <run dir>`, with the agent's workspace as the working
//!   directory, so the plan, the log and the transcript are the same files the
//!   default path produces, in the same places.
//! - **No secret in the unit.** A unit holds paths and a declared restart policy, and
//!   nothing else: the model key and the Forgejo token are read from the credentials
//!   file by the run itself, at the moment it needs them.
//! - **The state comes from systemd.** What the screens show for an agent in this
//!   mode is read back from `systemctl --user show`, never remembered from a start:
//!   a unit that is inactive is shown as not running, and one that is active is shown
//!   as running, whatever the tool last did.
//! - **A restart is declared and bounded, and it is not hidden.** The unit restarts a
//!   failed run (`Restart=on-failure`) within a bound (`StartLimitBurst` starts in
//!   `StartLimitIntervalSec`), and every restart systemd reports is written into the
//!   run's own log, so the record says what happened.
//!
//! This is a convenience, not a security boundary: the unit runs as the same user, in
//! the same workspace, with the same rights as the default mode. See the repository's
//! `docs/systemd-mode.md` for what a machine needs (a user session, and lingering for
//! an agent to outlive a logout) and for what this mode does not do.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::paths::AgentPaths;
use crate::run::{LOG_FILE, RunOutcome};
use crate::supervisor::State;
use crate::transcript;

/// The prefix of every unit this tool writes.
pub const UNIT_PREFIX: &str = "gewu-agent-";

/// The suffix of every unit this tool writes. These are services, not sockets.
pub const UNIT_SUFFIX: &str = ".service";

/// The file, in an agent's own directory, that records which mode it runs in.
pub const RECORD_FILE: &str = "supervision.toml";

/// The comment at the top of the record, for a person who opens it.
const RECORD_HEADER: &str = "\
# How this agent is supervised, written by gewu-multiverse.
#
# mode = \"tui\"      the agent is a child of the terminal UI: it stops when the UI
#                    exits, and the UI proves the process is gone.
# mode = \"systemd\"  the agent runs as a user unit, written by the tool and started
#                    with `systemctl --user`: it keeps running when the UI exits.
#
# run_dir is the run the unit currently points at, when the mode is systemd.
# With mode = \"tui\" this file is a record only; the default is the same as no file.
";

/// How an agent is supervised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A child of the terminal UI. The default, and what happens with no record.
    #[default]
    Tui,
    /// A user unit of the user's own systemd.
    Systemd,
}

impl Mode {
    /// The word written in the record and shown on the screens.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Tui => "tui",
            Mode::Systemd => "systemd",
        }
    }

    /// A phrase that completes "supervised by …".
    pub fn label(self) -> &'static str {
        match self {
            Mode::Tui => "this UI, as a child process",
            Mode::Systemd => "a user unit of your own systemd",
        }
    }

    /// The other mode, which is what a switch asks for.
    pub fn other(self) -> Mode {
        match self {
            Mode::Tui => Mode::Systemd,
            Mode::Systemd => Mode::Tui,
        }
    }
}

/// The record of one agent's supervision mode, as its file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supervision {
    /// The mode the agent runs in.
    pub mode: Mode,
    /// The run the unit points at, when the mode is [`Mode::Systemd`] and one was
    /// started. It is what lets a later session read the unit's state and outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_dir: Option<PathBuf>,
    /// How many restarts have already been written into the run's log, so a restart
    /// is recorded once and not once per poll.
    #[serde(default)]
    pub restarts_recorded: u32,
}

impl Default for Supervision {
    fn default() -> Self {
        Self {
            mode: Mode::Tui,
            run_dir: None,
            restarts_recorded: 0,
        }
    }
}

/// Where one agent's supervision record lives: inside the agent's own directory.
pub fn record_path(agent: &AgentPaths) -> PathBuf {
    agent.config_dir().join(RECORD_FILE)
}

/// Read one agent's record.
///
/// A missing, unreadable or malformed record is the default, which is the `tui` mode.
/// That is the safe direction: a record this tool cannot understand never causes a
/// unit to be started, and the behaviour is exactly what the installation did before
/// this mode existed.
pub fn read_record(agent: &AgentPaths) -> Supervision {
    match fs::read_to_string(record_path(agent)) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Supervision::default(),
    }
}

/// Write one agent's record, privately, replacing whatever was there.
pub fn write_record(agent: &AgentPaths, record: &Supervision) -> io::Result<()> {
    let body = toml::to_string(record).map_err(io::Error::other)?;
    let text = format!("{RECORD_HEADER}\n{body}");
    crate::mode::write_private(record_path(agent), text.as_bytes())
}

/// The user's own systemd unit directory: `$XDG_CONFIG_HOME/systemd/user`, or
/// `~/.config/systemd/user` when that is not set.
///
/// This is where `systemctl --user` looks, so it is the only directory a written
/// unit can live in and still be startable. It is the user's own directory: writing
/// here needs no privileges.
pub fn user_unit_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.config_dir().join("systemd").join("user"))
}

/// The unit name of one agent.
///
/// An agent name is already a safe, single path component of lowercase letters,
/// digits and hyphens ([`crate::naming`]), which is also what a systemd unit name
/// allows, so the unit name needs no escaping.
pub fn unit_name(agent: &str) -> String {
    format!("{UNIT_PREFIX}{agent}{UNIT_SUFFIX}")
}

/// The declared restart policy of every unit this tool writes.
///
/// A failed run is restarted, but not for ever: at most [`RestartPolicy::burst`]
/// starts within [`RestartPolicy::interval_secs`] seconds, then systemd leaves the
/// unit failed and the tool shows it as failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// How long systemd waits before restarting a failed run.
    pub restart_secs: u64,
    /// How many starts are allowed inside the interval.
    pub burst: u32,
    /// The interval the burst is counted over, in seconds.
    pub interval_secs: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            restart_secs: 2,
            burst: 3,
            interval_secs: 60,
        }
    }
}

/// Everything one unit file is written from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitSpec {
    /// The agent the unit is for.
    pub agent: String,
    /// The command the unit runs: the program and its arguments, in order.
    ///
    /// Production always builds this with [`UnitSpec::for_run`], which is exactly the
    /// command the terminal UI would run — `--run-agent <run dir>` — and the tests use
    /// it to run something harmless instead.
    pub exec: Vec<PathBuf>,
    /// The agent's workspace, which is the unit's working directory.
    pub workspace: PathBuf,
    /// The run's own log, which is where the unit's standard output and error go too.
    pub log: PathBuf,
    /// The declared restart policy.
    pub restart: RestartPolicy,
}

impl UnitSpec {
    /// The unit of one agent run: exactly the command the UI runs.
    pub fn for_run(
        agent: &str,
        binary: &Path,
        run_dir: &Path,
        workspace: &Path,
        log: &Path,
        restart: RestartPolicy,
    ) -> Self {
        Self {
            agent: agent.to_owned(),
            exec: vec![
                binary.to_path_buf(),
                PathBuf::from("--run-agent"),
                run_dir.to_path_buf(),
            ],
            workspace: workspace.to_path_buf(),
            log: log.to_path_buf(),
            restart,
        }
    }

    /// The unit name this spec is written under.
    pub fn unit_name(&self) -> String {
        unit_name(&self.agent)
    }
}

/// Render one word of a unit file, quoted the way systemd reads it.
///
/// A plain path is left as it is. Anything else is put in double quotes, with the
/// characters systemd would otherwise act on escaped: a backslash, a double quote, a
/// percent sign (a specifier) and a dollar sign (the start of a substitution). A line
/// break cannot be an argument, so it becomes a space.
pub fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._:@+-".contains(c));
    if plain {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Render the whole unit file for one spec.
///
/// This is a pure function of the spec: the same spec always renders the same bytes,
/// which is what makes it testable, and what makes "the unit holds no secret" a
/// property of the shape rather than of a review.
pub fn render_unit(spec: &UnitSpec) -> String {
    let command = spec
        .exec
        .iter()
        .map(|argument| quote(&argument.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let policy = spec.restart;
    format!(
        "\
# This file is written by gewu-multiverse for the agent `{agent}` in systemd mode.
# It holds no secret: the model key and the Forgejo token are read from the
# credentials file by the run itself. The tool rewrites this file at every start,
# so a change made here by hand is lost at the next one.

[Unit]
Description=gewu-multiverse agent `{agent}`
# A failed run is restarted, but not for ever: the start limit below bounds it.
StartLimitIntervalSec={interval}
StartLimitBurst={burst}

[Service]
Type=simple
# The agent's own workspace, exactly as the terminal UI would use it.
WorkingDirectory={workspace}
# Exactly the command the terminal UI runs for one agent run.
ExecStart={command}
# A run that fails is restarted, after {restart_secs} s, within the bound above.
Restart=on-failure
RestartSec={restart_secs}
# The run writes its own log and transcript; its streams go to that same log, so
# nothing it prints is lost when there is no terminal.
StandardOutput=append:{log}
StandardError=append:{log}
# A stop is polite first; the run's own timeout still bounds it from inside.
KillMode=mixed
TimeoutStopSec=10

[Install]
WantedBy=default.target
",
        agent = spec.agent,
        burst = policy.burst,
        interval = policy.interval_secs,
        workspace = quote(&spec.workspace.to_string_lossy()),
        command = command,
        restart_secs = policy.restart_secs,
        log = quote(&spec.log.to_string_lossy()),
    )
}

/// Write one unit file into a unit directory, returning its path.
///
/// The directory is created if it is absent and held to mode 0700. The unit file is
/// mode 0644: it is not a secret, and only the user's own systemd reads it.
pub fn write_unit(dir: &Path, spec: &UnitSpec) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    crate::mode::set_mode(dir, crate::mode::PRIVATE_DIR_MODE)?;
    let path = dir.join(spec.unit_name());
    let text = render_unit(spec);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

/// The `systemctl --user` client: one verb, one call, one answer.
///
/// Nothing here decides anything; the answers are parsed by [`parse_show`] and turned
/// into screens by [`state_of`], which are the parts the tests can hold to.
#[derive(Debug, Clone)]
pub struct Systemctl {
    binary: PathBuf,
}

impl Default for Systemctl {
    fn default() -> Self {
        Self::new()
    }
}

impl Systemctl {
    /// A client that runs `systemctl` from the path.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from("systemctl"),
        }
    }

    /// A client that runs a chosen program instead, which is what the tests do.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// The program this client runs.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Run one verb of the user's systemd, as this user.
    fn run(&self, verb: &str, arguments: &[&str]) -> io::Result<Output> {
        Command::new(&self.binary)
            .arg("--user")
            .arg(verb)
            .args(arguments)
            .output()
    }

    /// Run one verb that prints something, and hand back what it printed.
    fn ask(&self, verb: &str, arguments: &[&str], what: &str) -> Result<String, String> {
        let output = self
            .run(verb, arguments)
            .map_err(|error| format!("{what}: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!("{what}: {}", first_line(&output.stderr)))
        }
    }

    /// Run one verb that succeeds or fails, and keep nothing it prints.
    fn act(&self, verb: &str, arguments: &[&str], what: &str) -> Result<(), String> {
        let output = self
            .run(verb, arguments)
            .map_err(|error| format!("{what}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("{what}: {}", first_line(&output.stderr)))
        }
    }

    /// Whether the user's systemd answers at all, and which version it is.
    ///
    /// A machine without a user session (a bare container, a serial console with no
    /// login manager) has no user manager to talk to; this is the question that tells
    /// the two apart, and it is asked before a unit is written.
    pub fn available(&self) -> Result<String, String> {
        let version = self.ask(
            "show",
            &["-p", "Version", "--value"],
            "the user's systemd cannot be reached",
        )?;
        let version = version.trim();
        if version.is_empty() {
            return Err("the user's systemd cannot be reached: it answered nothing".to_owned());
        }
        Ok(version.to_owned())
    }

    /// Ask systemd to read the unit files again.
    pub fn daemon_reload(&self) -> Result<(), String> {
        self.act("daemon-reload", &[], "systemd could not read its units again")
    }

    /// Ask systemd to start a unit.
    pub fn start(&self, unit: &str) -> Result<(), String> {
        self.act("start", &[unit], &format!("`{unit}` could not be started"))
    }

    /// Ask systemd to stop a unit.
    pub fn stop(&self, unit: &str) -> Result<(), String> {
        self.act("stop", &[unit], &format!("`{unit}` could not be stopped"))
    }

    /// Ask systemd to start a unit at every login.
    pub fn enable(&self, unit: &str) -> Result<(), String> {
        self.act("enable", &[unit], &format!("`{unit}` could not be enabled"))
    }

    /// Ask systemd to stop starting a unit at every login.
    pub fn disable(&self, unit: &str) -> Result<(), String> {
        self.act("disable", &[unit], &format!("`{unit}` could not be disabled"))
    }

    /// Clear a unit's failed state, so a unit the tool no longer manages stops being
    /// reported as failed.
    pub fn reset_failed(&self, unit: &str) -> Result<(), String> {
        self.act(
            "reset-failed",
            &[unit],
            &format!("`{unit}` could not be reset"),
        )
    }

    /// The whole set of properties systemd reports for one unit.
    pub fn show(&self, unit: &str) -> Result<UnitStatus, String> {
        let text = self.ask("show", &[unit], &format!("systemd could not report `{unit}`"))?;
        Ok(UnitStatus::from_map(&parse_show(&text)))
    }
}

/// The first line of a message, for a one-line status.
fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().find(|line| !line.trim().is_empty());
    match line {
        Some(line) => line.trim().to_owned(),
        None => "systemd gave no reason".to_owned(),
    }
}

/// Parse the `Key=Value` lines `systemctl show` prints.
pub fn parse_show(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_owned(), value.to_owned());
        }
    }
    map
}

/// What systemd says about one unit, reduced to the properties the screens use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitStatus {
    /// `loaded`, `not-found`, `error`, …
    pub load_state: String,
    /// `active`, `inactive`, `failed`, `activating`, `deactivating`, `reloading`.
    pub active_state: String,
    /// `running`, `dead`, `failed`, `auto-restart`, …
    pub sub_state: String,
    /// The main process's id, when there is one.
    pub main_pid: Option<u32>,
    /// How many times systemd has restarted this unit.
    pub n_restarts: u32,
    /// `success`, `exit-code`, `signal`, `timeout`, …
    pub result: String,
    /// The main process's exit code, when there is one.
    pub exec_main_status: Option<i32>,
    /// When the unit entered its active state, in microseconds since boot.
    pub active_enter_monotonic_us: Option<u64>,
}

impl UnitStatus {
    /// Read the properties out of a parsed `systemctl show`.
    pub fn from_map(map: &BTreeMap<String, String>) -> Self {
        let number = |key: &str| -> Option<u64> {
            map.get(key)
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value != 0)
        };
        Self {
            load_state: map.get("LoadState").cloned().unwrap_or_default(),
            active_state: map.get("ActiveState").cloned().unwrap_or_default(),
            sub_state: map.get("SubState").cloned().unwrap_or_default(),
            main_pid: number("MainPID").map(|pid| pid as u32),
            n_restarts: number("NRestarts").unwrap_or(0) as u32,
            result: map.get("Result").cloned().unwrap_or_default(),
            exec_main_status: map
                .get("ExecMainStatus")
                .and_then(|value| value.trim().parse::<i32>().ok()),
            active_enter_monotonic_us: number("ActiveEnterTimestampMonotonic"),
        }
    }

    /// Whether systemd considers this unit to be alive right now: running, starting,
    /// stopping or reloading. This is deliberately the same set the screens call alive,
    /// so a unit that still has a process is never reported as nothing to stop.
    pub fn is_live(&self) -> bool {
        matches!(
            self.active_state.as_str(),
            "active" | "activating" | "reloading" | "deactivating"
        )
    }

}

/// One run a unit points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRun {
    /// The run's identity.
    pub run_id: String,
    /// The run's directory, which holds its log and transcript.
    pub run_dir: PathBuf,
}

/// The uptime of the machine, in microseconds, read from Linux's own `/proc`.
///
/// This is what turns systemd's "active since" (microseconds since boot) into the
/// elapsed time the screens show. Where it cannot be read, the elapsed time is
/// reported as zero rather than guessed.
pub fn read_uptime_us() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .next()
                .and_then(|seconds| seconds.parse::<f64>().ok())
        })
        .map(|seconds| (seconds * 1_000_000.0) as u64)
        .unwrap_or(0)
}

/// Map what systemd reports to what the screens show.
///
/// The rule the whole module rests on: a unit that systemd does not call alive is not
/// shown as running, and a unit it does call alive is not shown as stopped. Where a
/// run has finished, its outcome is read from that run's own transcript, so the two
/// paths — a child of the UI and a unit — produce the same kind of record.
pub fn state_of(status: &UnitStatus, run: Option<&UnitRun>, uptime_us: u64) -> State {
    let run_id = run.map(|run| run.run_id.clone()).unwrap_or_default();
    let pid = status.main_pid.unwrap_or(0);
    let elapsed_ms = elapsed_ms(status, uptime_us);

    // No unit file of ours is there: nothing can be running under this name.
    if status.load_state == "not-found" || status.load_state.is_empty() {
        return State::Idle;
    }
    match status.active_state.as_str() {
        "active" | "reloading" => State::Running {
            run_id,
            pid,
            elapsed_ms,
        },
        "activating" => State::Starting { run_id, pid },
        "deactivating" => State::Stopping {
            run_id,
            pid,
            elapsed_ms,
        },
        "failed" => {
            let (outcome, reason, duration_ms) = transcript_outcome(run).unwrap_or((
                RunOutcome::Failed,
                Some(failed_reason(status)),
                0,
            ));
            State::Finished {
                run_id,
                outcome,
                reason,
                duration_ms,
            }
        }
        _ => match transcript_outcome(run) {
            Some((outcome, reason, duration_ms)) => State::Finished {
                run_id,
                outcome,
                reason,
                duration_ms,
            },
            // There is a run on record, the unit is not active, and the run left no
            // terminal record: it was killed without a chance to write one. Saying so
            // is more honest than inventing a success or a stop.
            None if run.is_some() => State::Finished {
                run_id,
                outcome: RunOutcome::Failed,
                reason: Some(
                    "the unit is not active and the run left no terminal record".to_owned(),
                ),
                duration_ms: 0,
            },
            None => State::Idle,
        },
    }
}

/// The run's own terminal record, when it has one.
fn transcript_outcome(run: Option<&UnitRun>) -> Option<(RunOutcome, Option<String>, u64)> {
    let run = run?;
    match transcript::terminal_record(&run.run_dir)? {
        transcript::Record::RunFinished {
            outcome,
            reason,
            duration_ms,
            ..
        } => Some((
            RunOutcome::parse(&outcome).unwrap_or(RunOutcome::Failed),
            reason,
            duration_ms,
        )),
        _ => None,
    }
}

/// Why a failed unit failed, when the run itself left no record.
fn failed_reason(status: &UnitStatus) -> String {
    let mut reason = format!("the unit failed (systemd says `{}`)", status.result);
    if let Some(code) = status.exec_main_status {
        reason.push_str(&format!(", exit code {code}"));
    }
    reason
}

/// How long the unit has been in its current active state.
fn elapsed_ms(status: &UnitStatus, uptime_us: u64) -> u64 {
    match status.active_enter_monotonic_us {
        Some(started) => uptime_us.saturating_sub(started) / 1000,
        None => 0,
    }
}

/// One systemd-supervised agent, as the supervisor holds it.
#[derive(Debug)]
struct RunRecord {
    run_id: String,
    run_dir: PathBuf,
    state: State,
    restarts_recorded: u32,
}

/// The systemd half of the supervisor: writes units, starts and stops them, and
/// reads their state back.
#[derive(Debug)]
pub struct SystemdSupervisor {
    systemctl: Systemctl,
    unit_dir: PathBuf,
    policy: RestartPolicy,
    runs: HashMap<String, RunRecord>,
    last_poll: Option<Instant>,
    poll_every: Duration,
    last_error: Option<String>,
}

impl SystemdSupervisor {
    /// A systemd supervisor over one unit directory.
    pub fn new(systemctl: Systemctl, unit_dir: PathBuf, policy: RestartPolicy) -> Self {
        Self {
            systemctl,
            unit_dir,
            policy,
            runs: HashMap::new(),
            last_poll: None,
            poll_every: Duration::from_secs(1),
            last_error: None,
        }
    }

    /// How often the units are read back, so the UI is not asked of systemd per frame.
    pub fn set_poll_every(&mut self, every: Duration) {
        self.poll_every = every;
    }

    /// The directory units are written into.
    pub fn unit_dir(&self) -> &Path {
        &self.unit_dir
    }

    /// Write units into a different directory. Tests use this; production leaves the
    /// directory where `systemctl --user` looks for it.
    pub fn set_unit_dir(&mut self, unit_dir: PathBuf) {
        self.unit_dir = unit_dir;
    }

    /// Change the program used for `systemctl`, which is what the tests do.
    pub fn set_systemctl(&mut self, systemctl: Systemctl) {
        self.systemctl = systemctl;
    }

    /// Whether the user's systemd answers, and its version.
    pub fn available(&self) -> Result<String, String> {
        self.systemctl.available()
    }

    /// The state of one agent, when this supervisor knows about it.
    pub fn state(&self, agent: &str) -> Option<&State> {
        self.runs.get(agent).map(|run| &run.state)
    }

    /// Whether one agent's unit is alive according to the last read.
    pub fn is_live(&self, agent: &str) -> bool {
        self.runs
            .get(agent)
            .map(|run| run.state.is_live())
            .unwrap_or(false)
    }

    /// How many agents' units are alive.
    pub fn live_count(&self) -> usize {
        self.runs.values().filter(|run| run.state.is_live()).count()
    }

    /// How many restarts of one agent's current run the last read showed.
    pub fn restart_count(&self, agent: &str) -> u32 {
        self.runs
            .get(agent)
            .map(|run| run.restarts_recorded)
            .unwrap_or(0)
    }

    /// The last problem systemd gave, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Remember the run a record names, so a later session can read its state.
    pub fn note_run(&mut self, agent: &str, run_dir: Option<PathBuf>, restarts_recorded: u32) {
        let Some(run_dir) = run_dir else {
            return;
        };
        let run_id = run_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        self.runs.insert(
            agent.to_owned(),
            RunRecord {
                run_id,
                run_dir,
                state: State::Idle,
                restarts_recorded,
            },
        );
    }

    /// The run one agent's unit points at, when one is known.
    pub fn run_of(&self, agent: &str) -> Option<UnitRun> {
        self.runs.get(agent).map(|run| UnitRun {
            run_id: run.run_id.clone(),
            run_dir: run.run_dir.clone(),
        })
    }

    /// Forget the runs of agents that are not supervised by a unit any more, so a
    /// stale run cannot make it look as though something of that agent is running.
    pub fn keep_only(&mut self, agents: &[String]) {
        self.runs.retain(|agent, _| agents.contains(agent));
    }

    /// Write the unit for one run, ask systemd to read it again, and start it.
    ///
    /// The unit is written before it is started, so what is started is what is on
    /// disk; a failure at any step is a sentence, never a silent fallback to the
    /// default mode.
    pub fn start(&mut self, spec: &UnitSpec, run_id: String, run_dir: PathBuf) -> Result<u32, String> {
        self.systemctl.available()?;
        let unit = spec.unit_name();
        write_unit(&self.unit_dir, spec)
            .map_err(|error| format!("the unit file {unit} cannot be written: {error}"))?;
        self.systemctl.daemon_reload()?;
        self.systemctl.start(&unit)?;
        self.runs.insert(
            spec.agent.clone(),
            RunRecord {
                run_id: run_id.clone(),
                run_dir,
                state: State::Starting { run_id, pid: 0 },
                restarts_recorded: 0,
            },
        );
        // Read the state back at once, so the screens never show a start that systemd
        // did not actually make.
        self.poll_now();
        Ok(self
            .runs
            .get(&spec.agent)
            .and_then(|run| match &run.state {
                State::Running { pid, .. } | State::Stopping { pid, .. } => Some(*pid),
                State::Starting { pid, .. } => Some(*pid),
                _ => None,
            })
            .unwrap_or(0))
    }

    /// Stop one agent's unit, and record that the run was stopped.
    ///
    /// Returns whether there was something alive to stop. A unit that was not active
    /// is left alone: there is nothing to signal, and saying otherwise would be a lie.
    pub fn stop(&mut self, agent: &str) -> Result<bool, String> {
        self.stop_with_reason(agent, "the user unit was stopped on request")
    }

    /// Stop one agent's unit and record the run with a caller-chosen reason, so a
    /// stop the user never asked for — a configuration re-read that no longer lists
    /// the agent — is not recorded as "on request" (T-6).
    pub fn stop_with_reason(&mut self, agent: &str, reason_text: &str) -> Result<bool, String> {
        let unit = unit_name(agent);
        let status = self.systemctl.show(&unit)?;
        if !status.is_live() {
            self.refresh(agent, &status);
            return Ok(false);
        }
        self.systemctl.stop(&unit)?;
        let run = self.runs.get(agent);
        if let Some(record) = run {
            let (run_id, run_dir) = (record.run_id.clone(), record.run_dir.clone());
            let (outcome, reason, duration_ms) = crate::supervisor::record_outcome_if_missing(
                &run_dir,
                &run_id,
                RunOutcome::Stopped,
                Some(reason_text.to_owned()),
                0,
            );
            if let Some(record) = self.runs.get_mut(agent) {
                record.state = State::Finished {
                    run_id,
                    outcome,
                    reason,
                    duration_ms,
                };
            }
        }
        // Read the state back: the unit is inactive now, and the screens must say so.
        self.poll_now();
        Ok(true)
    }

    /// Stop one agent's unit if it is alive, without recording an outcome. Used when
    /// the tool removes a unit on a mode switch.
    pub fn stop_quietly(&mut self, agent: &str) -> Result<(), String> {
        let unit = unit_name(agent);
        let status = self.systemctl.show(&unit)?;
        if status.is_live() {
            self.systemctl.stop(&unit)?;
        }
        Ok(())
    }

    /// Remove one agent's unit file, after making sure it is not running.
    ///
    /// An agent with no unit file on disk has nothing to stop and nothing systemd
    /// needs to be asked about, so leaving `systemd` mode for such an agent succeeds
    /// even when the user manager cannot be reached. That is the way back out of a
    /// `systemd` record written by an earlier session on a machine that has since
    /// lost its user session (T-1): the record, not the manager, was the door.
    pub fn remove_unit(&mut self, agent: &str) -> Result<(), String> {
        let path = self.unit_dir.join(unit_name(agent));
        let on_disk = path.is_file();
        if on_disk {
            self.stop_quietly(agent)?;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("the unit file {} cannot be removed: {error}", path.display()));
            }
        }
        self.runs.remove(agent);
        if on_disk {
            // A unit the tool no longer manages must not stay reported as failed.
            let _ = self.systemctl.reset_failed(&unit_name(agent));
            self.systemctl.daemon_reload()?;
        }
        Ok(())
    }

    /// Read every unit back, at most once per poll interval.
    pub fn poll(&mut self, now: Instant) {
        let due = match self.last_poll {
            Some(at) => now.duration_since(at) >= self.poll_every,
            None => true,
        };
        if !due {
            return;
        }
        self.poll_now();
    }

    /// Read every unit back now.
    pub fn poll_now(&mut self) {
        self.last_poll = Some(Instant::now());
        let agents: Vec<String> = self.runs.keys().cloned().collect();
        for agent in agents {
            let unit = unit_name(&agent);
            match self.systemctl.show(&unit) {
                Ok(status) => {
                    self.record_restarts(&agent, &status);
                    self.refresh(&agent, &status);
                    self.last_error = None;
                }
                Err(message) => {
                    // systemd cannot answer. The state is not "running" and not
                    // "stopped": it is unknown, and the screens say so.
                    if let Some(record) = self.runs.get_mut(&agent) {
                        record.state = State::Unavailable {
                            reason: message.clone(),
                        };
                    }
                    self.last_error = Some(message);
                }
            }
        }
    }

    /// Put what systemd reports into the agent's state.
    fn refresh(&mut self, agent: &str, status: &UnitStatus) {
        let uptime_us = read_uptime_us();
        let run = self.runs.get(agent).map(|record| UnitRun {
            run_id: record.run_id.clone(),
            run_dir: record.run_dir.clone(),
        });
        let state = state_of(status, run.as_ref(), uptime_us);
        if let Some(record) = self.runs.get_mut(agent) {
            record.state = state;
        }
    }

    /// Write a restart into the run's own log, once per restart systemd reports.
    ///
    /// The record is the run's log, the same file the default path writes, so a
    /// restart is visible to anyone reading the run afterwards rather than only to a
    /// screen that happened to be open.
    fn record_restarts(&mut self, agent: &str, status: &UnitStatus) {
        let Some(record) = self.runs.get_mut(agent) else {
            return;
        };
        if status.n_restarts <= record.restarts_recorded {
            return;
        }
        let line = format!(
            "{} supervisor: systemd restarted this run (restart {} of at most {})\n",
            crate::run::started_stamp(),
            status.n_restarts,
            self.policy.burst,
        );
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(record.run_dir.join(LOG_FILE))
        {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
        record.restarts_recorded = status.n_restarts;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Layout;
    use crate::test_support::scratch_root;
    use crate::transcript::{Record, Transcript};

    fn spec(tag: &str) -> (UnitSpec, PathBuf) {
        let root = scratch_root(tag);
        let run_dir = root.join("state").join("scribe").join("run-1-0001");
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let spec = UnitSpec::for_run(
            "scribe",
            Path::new("/usr/local/bin/gewu-multiverse"),
            &run_dir,
            &root.join("agents").join("scribe").join("workspace"),
            &run_dir.join(LOG_FILE),
            RestartPolicy::default(),
        );
        (spec, run_dir)
    }

    #[test]
    fn a_unit_name_is_derived_from_the_agent_and_is_a_safe_file_name() {
        assert_eq!(unit_name("scribe"), "gewu-agent-scribe.service");
        assert_eq!(unit_name("agent-1"), "gewu-agent-agent-1.service");
        assert_eq!(Path::new(&unit_name("scribe")).components().count(), 1);
    }

    #[test]
    fn the_same_spec_always_renders_the_same_unit() {
        let (spec, _) = spec("unit-deterministic");
        assert_eq!(render_unit(&spec), render_unit(&spec));
    }

    #[test]
    fn a_rendered_unit_runs_exactly_the_command_the_ui_would_run() {
        let (spec, run_dir) = spec("unit-command");
        let text = render_unit(&spec);
        let expected = format!(
            "ExecStart=/usr/local/bin/gewu-multiverse --run-agent {}",
            run_dir.display()
        );
        assert!(text.contains(&expected), "{text}");
        assert!(text.contains("WorkingDirectory="), "{text}");
        assert!(text.contains("Type=simple"), "{text}");
    }

    #[test]
    fn a_rendered_unit_declares_the_restart_policy() {
        let (spec, _) = spec("unit-restart");
        let text = render_unit(&spec);
        assert!(text.contains("Restart=on-failure"), "{text}");
        assert!(text.contains("RestartSec=2"), "{text}");
        assert!(text.contains("StartLimitBurst=3"), "{text}");
        assert!(text.contains("StartLimitIntervalSec=60"), "{text}");
    }

    #[test]
    fn a_rendered_unit_sends_the_run_streams_to_the_run_log() {
        let (spec, run_dir) = spec("unit-streams");
        let text = render_unit(&spec);
        let log = run_dir.join(LOG_FILE);
        assert!(
            text.contains(&format!("StandardOutput=append:{}", log.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("StandardError=append:{}", log.display())),
            "{text}"
        );
    }

    #[test]
    fn no_secret_can_reach_a_unit_file() {
        // The shape is the proof: every line that is not a comment is one of the
        // settings above, and no setting carries a key or a token. The one secret-like
        // string this test can think of is put where a secret would live (a credentials
        // path) and must not appear in the unit.
        let (spec, _) = spec("unit-sealed");
        let text = render_unit(&spec);
        let allowed = [
            "[Unit]",
            "[Service]",
            "[Install]",
            "Description=",
            "StartLimitIntervalSec=",
            "StartLimitBurst=",
            "Type=",
            "WorkingDirectory=",
            "ExecStart=",
            "Restart=",
            "RestartSec=",
            "StandardOutput=",
            "StandardError=",
            "KillMode=",
            "TimeoutStopSec=",
            "WantedBy=",
        ];
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert!(
                allowed.iter().any(|setting| line.starts_with(setting)),
                "the unit holds a line that is not one of its settings: {line}"
            );
            let lowered = line.to_ascii_lowercase();
            for forbidden in ["model_keys", "token", "key=", "password", "secret"] {
                assert!(
                    !lowered.contains(forbidden),
                    "the unit must not hold `{forbidden}`: {line}"
                );
            }
        }
        assert!(!text.contains("gw-fixture-key"), "{text}");
        assert!(!text.contains("credentials.toml"), "{text}");
    }

    #[test]
    fn a_path_that_needs_quoting_is_quoted_and_escaped() {
        assert_eq!(quote("/home/a/b"), "/home/a/b");
        assert_eq!(quote("/home/a b"), "\"/home/a b\"");
        assert_eq!(quote("/a/100%"), "\"/a/100%%\"");
        assert_eq!(quote("/a/$HOME"), "\"/a/$$HOME\"");
        assert_eq!(quote("/a/\"q\""), "\"/a/\\\"q\\\"\"");
        assert_eq!(quote("/a\nb"), "\"/a b\"");
    }

    #[test]
    fn a_unit_with_a_space_in_a_path_still_names_one_command() {
        let spec = UnitSpec {
            agent: "scribe".to_owned(),
            exec: vec![
                PathBuf::from("/opt/gewu multiverse/bin/gewu-multiverse"),
                PathBuf::from("--run-agent"),
                PathBuf::from("/var/lib/gewu/run 1"),
            ],
            workspace: PathBuf::from("/var/lib/gewu/work space"),
            log: PathBuf::from("/var/lib/gewu/run 1/run.log"),
            restart: RestartPolicy::default(),
        };
        let text = render_unit(&spec);
        assert!(
            text.contains(
                "ExecStart=\"/opt/gewu multiverse/bin/gewu-multiverse\" --run-agent \"/var/lib/gewu/run 1\""
            ),
            "{text}"
        );
        assert!(
            text.contains("WorkingDirectory=\"/var/lib/gewu/work space\""),
            "{text}"
        );
    }

    fn status(active: &str, load: &str) -> UnitStatus {
        UnitStatus {
            load_state: load.to_owned(),
            active_state: active.to_owned(),
            ..UnitStatus::default()
        }
    }

    #[test]
    fn an_active_unit_is_running_with_systemds_pid_and_elapsed_time() {
        let status = UnitStatus {
            load_state: "loaded".to_owned(),
            active_state: "active".to_owned(),
            sub_state: "running".to_owned(),
            main_pid: Some(4321),
            active_enter_monotonic_us: Some(1_000_000),
            ..UnitStatus::default()
        };
        let run = UnitRun {
            run_id: "run-1-0001".to_owned(),
            run_dir: PathBuf::from("/nonexistent"),
        };
        match state_of(&status, Some(&run), 3_500_000) {
            State::Running {
                run_id,
                pid,
                elapsed_ms,
            } => {
                assert_eq!(run_id, "run-1-0001");
                assert_eq!(pid, 4321);
                assert_eq!(elapsed_ms, 2500);
            }
            other => panic!("expected running, got {other:?}"),
        }
    }

    #[test]
    fn an_activating_unit_is_starting_and_a_deactivating_one_is_stopping() {
        let mut activating = status("activating", "loaded");
        activating.main_pid = Some(7);
        assert!(matches!(
            state_of(&activating, None, 0),
            State::Starting { pid: 7, .. }
        ));
        let deactivating = status("deactivating", "loaded");
        assert!(matches!(
            state_of(&deactivating, None, 0),
            State::Stopping { .. }
        ));
    }

    #[test]
    fn a_unit_waiting_to_restart_is_shown_as_starting_not_as_stopped() {
        let status = UnitStatus {
            load_state: "loaded".to_owned(),
            active_state: "activating".to_owned(),
            sub_state: "auto-restart".to_owned(),
            ..UnitStatus::default()
        };
        assert!(matches!(state_of(&status, None, 0), State::Starting { .. }));
    }

    #[test]
    fn a_unit_that_does_not_exist_is_idle() {
        let status = status("inactive", "not-found");
        assert_eq!(state_of(&status, None, 0), State::Idle);
    }

    #[test]
    fn an_inactive_unit_with_no_run_is_idle() {
        let status = status("inactive", "loaded");
        assert_eq!(state_of(&status, None, 0), State::Idle);
    }

    #[test]
    fn an_inactive_unit_with_a_finished_run_shows_that_runs_outcome() {
        let root = scratch_root("systemd-inactive-finished");
        let run_dir = root.join("run-9-0001");
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
        transcript
            .append(&Record::run_started("run-9-0001", "scribe", 5, "/w"))
            .expect("a record is appended");
        transcript
            .append(&Record::run_finished(
                "run-9-0001",
                RunOutcome::Succeeded,
                Some(0),
                42,
                None,
            ))
            .expect("a record is appended");
        let run = UnitRun {
            run_id: "run-9-0001".to_owned(),
            run_dir,
        };
        match state_of(&status("inactive", "loaded"), Some(&run), 0) {
            State::Finished {
                outcome,
                duration_ms,
                ..
            } => {
                assert_eq!(outcome, RunOutcome::Succeeded);
                assert_eq!(duration_ms, 42);
            }
            other => panic!("expected a finished run, got {other:?}"),
        }
    }

    #[test]
    fn an_inactive_unit_whose_run_left_no_record_says_so() {
        let root = scratch_root("systemd-inactive-norecord");
        let run_dir = root.join("run-9-0002");
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let run = UnitRun {
            run_id: "run-9-0002".to_owned(),
            run_dir,
        };
        match state_of(&status("inactive", "loaded"), Some(&run), 0) {
            State::Finished {
                outcome, reason, ..
            } => {
                assert_eq!(outcome, RunOutcome::Failed);
                assert!(reason.expect("a reason").contains("no terminal record"));
            }
            other => panic!("expected a finished run, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_unit_is_a_failure_with_systemds_reason() {
        let status = UnitStatus {
            load_state: "loaded".to_owned(),
            active_state: "failed".to_owned(),
            result: "exit-code".to_owned(),
            exec_main_status: Some(3),
            ..UnitStatus::default()
        };
        match state_of(&status, None, 0) {
            State::Finished {
                outcome, reason, ..
            } => {
                assert_eq!(outcome, RunOutcome::Failed);
                let reason = reason.expect("a reason");
                assert!(reason.contains("exit-code"), "{reason}");
                assert!(reason.contains('3'), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn show_output_is_parsed_into_properties() {
        let text = "LoadState=loaded\nActiveState=active\nMainPID=99\nResult=success\n";
        let map = parse_show(text);
        assert_eq!(map.get("ActiveState").map(String::as_str), Some("active"));
        assert_eq!(map.get("MainPID").map(String::as_str), Some("99"));
        let status = UnitStatus::from_map(&map);
        assert_eq!(status.main_pid, Some(99));
        assert!(status.is_live());
    }

    #[test]
    fn a_mode_record_round_trips_and_an_absent_one_is_the_tui_default() {
        let root = scratch_root("supervision-record");
        let layout = Layout::rooted_at(&root);
        let agent = layout.ensure_agent("scribe").expect("the agent is prepared");
        assert_eq!(read_record(&agent), Supervision::default());
        assert_eq!(read_record(&agent).mode, Mode::Tui);

        let record = Supervision {
            mode: Mode::Systemd,
            run_dir: Some(PathBuf::from("/state/scribe/run-1")),
            restarts_recorded: 2,
        };
        write_record(&agent, &record).expect("the record is written");
        assert_eq!(read_record(&agent), record);
        let mode = crate::mode::mode_of(&record_path(&agent)).expect("the mode reads");
        assert_eq!(mode, crate::mode::SECRET_FILE_MODE);
    }

    #[test]
    fn a_malformed_record_reads_as_the_default_mode() {
        let root = scratch_root("supervision-malformed");
        let layout = Layout::rooted_at(&root);
        let agent = layout.ensure_agent("scribe").expect("the agent is prepared");
        fs::write(record_path(&agent), "mode = \"something-else\"\n").expect("written");
        assert_eq!(read_record(&agent).mode, Mode::Tui);
    }

    #[test]
    fn write_unit_puts_the_file_where_systemd_looks() {
        let root = scratch_root("write-unit");
        let dir = root.join("user");
        let (spec, _) = spec("write-unit-content");
        let path = write_unit(&dir, &spec).expect("the unit is written");
        assert_eq!(path, dir.join("gewu-agent-scribe.service"));
        assert_eq!(
            fs::read_to_string(&path).expect("the unit reads"),
            render_unit(&spec)
        );
    }

    #[test]
    fn a_client_with_no_systemctl_says_so_rather_than_pretending() {
        let client = Systemctl::with_binary("/nonexistent/systemctl-not-here");
        let error = client.available().expect_err("no systemd to reach");
        assert!(error.contains("cannot be reached"), "{error}");
    }
}
