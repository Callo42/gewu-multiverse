//! The supervisor: the part of the tool that starts, watches and stops agents.
//!
//! The terminal UI owns exactly one supervisor. It starts one child process per
//! agent, reads that process's state, stops it when asked, and stops it when it
//! overruns its timeout. It also holds the concurrency limit: at most a configured
//! number of agents run at once, and a start beyond that limit waits its turn
//! instead of failing.
//!
//! Two rules come straight from how this tool is meant to behave.
//!
//! - **One child process per agent, in its own process group.** The child is started
//!   with the agent's workspace as its working directory and as the leader of a new
//!   process group, so signalling the group reaches the tools the agent started. A
//!   stop is polite first and forceful after a grace, and the group is swept again
//!   when the run ends, so a run leaves nothing of itself behind.
//! - **The process decides, not its output.** An exit of zero is a success; any other
//!   exit is a failure; a run the tool stopped is `stopped`; a run the tool stopped
//!   for its timeout is `timed_out`. What the process wrote is a log, never a verdict.
//!
//! An agent may instead be supervised by the user's own systemd, as a unit this tool
//! writes ([`crate::systemd`]). The same supervisor holds both: it routes a start,
//! a stop and a state read to the mode the agent is in, so a caller never has to know
//! which one is in use. A unit is deliberately *not* stopped when the UI exits: that
//! is the whole point of the mode.
//!
//! Nothing here runs as root, installs a system service or uses a container runtime.
//! The child is an ordinary process of the same user, and so is a unit.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::paths::AgentPaths;
use crate::run::{LOG_FILE, RunModel, RunOutcome, RunPlan, new_run_id};
use crate::systemd::{
    self, Mode, RestartPolicy, Supervision, Systemctl, SystemdSupervisor, UnitSpec,
};
use crate::transcript::{self, Record, Transcript};

/// How long a stopped run may take to end politely before it is ended forcefully.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// The most significant part of what the screens show about one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Nothing is running and nothing has run in this session.
    Idle,
    /// The start is waiting for a free slot, behind `position` starts.
    Waiting {
        /// The identity the run will have.
        run_id: String,
        /// Its place in the line, starting at one.
        position: usize,
    },
    /// The process was started and has not been seen running yet.
    Starting {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
    },
    /// The process is running.
    Running {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
        /// How long it has been running, in milliseconds.
        elapsed_ms: u64,
    },
    /// A stop was asked for and the process is being ended.
    Stopping {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
        /// How long it has been running, in milliseconds.
        elapsed_ms: u64,
    },
    /// The newest run record of this agent is there but could not be read.
    ///
    /// This is not `idle`: `idle` is the word for "no run", and pretending a record
    /// the product cannot read is the same as no record is the class W38-1/W41-2
    /// names. The word is the Runs row's own [`crate::run::UNREADABLE_WORD`].
    Unreadable {
        /// The run's identity.
        run_id: String,
        /// Why the record could not be read, for the note area.
        reason: String,
    },
    /// The last run of this agent is over.
    Finished {
        /// The run's identity.
        run_id: String,
        /// How it ended.
        outcome: RunOutcome,
        /// Why, when there is a reason to give.
        reason: Option<String>,
        /// How long it lasted, in milliseconds.
        duration_ms: u64,
    },
    /// The agent runs as a user unit, and systemd could not be asked about it, so
    /// whether it is running is not known here. This is never shown as running and
    /// never shown as stopped: the screens say the state is unknown.
    Unavailable {
        /// Why, in a sentence.
        reason: String,
    },
}

impl State {
    /// One word for a table cell.
    pub fn word(&self) -> String {
        match self {
            State::Idle => "idle".to_owned(),
            State::Waiting { position, .. } => format!("waiting ({position} in line)"),
            State::Starting { .. } => "starting".to_owned(),
            State::Running { elapsed_ms, .. } => crate::run::running_word(*elapsed_ms),
            State::Stopping { elapsed_ms, .. } => {
                format!("stopping ({}s)", elapsed_ms / 1000)
            }
            State::Unreadable { .. } => crate::run::UNREADABLE_WORD.to_owned(),
            State::Finished { outcome, .. } => {
                // One word for one outcome, on every screen (W39-2): the hub prints the
                // outcome's own word, the same string the Runs screen reads from
                // `RunOutcome::as_str`, so `timed_out` cannot become `timed out` here.
                outcome.as_str().to_owned()
            }
            State::Unavailable { .. } => "state unknown".to_owned(),
        }
    }

    /// One word for a table cell, with the failure class when the reason names one.
    ///
    /// A person looking at the list after a failure needs more than `failed`: the
    /// class (`transport`, `missing_key`, …) is what tells them what to change.
    pub fn word_with_reason(&self) -> String {
        match self {
            State::Finished {
                outcome: RunOutcome::Failed,
                reason: Some(reason),
                ..
            } => match failure_class(reason) {
                Some(class) => format!("failed ({class})"),
                None => "failed".to_owned(),
            },
            _ => self.word(),
        }
    }

    /// One word for a table cell on the hub, where the column is narrow.
    ///
    /// The failure class is shortened to a word that fits whole — the page map's own
    /// rule for a state column — rather than ellipsised into a fragment
    /// (`failed (s…er_error)`, L-6). `missing_key` already became `key`; every class
    /// the model and Forgejo clients produce now has a whole word too. The Agent
    /// screen keeps the long form, which fits there (E-7, P-5).
    pub fn cell_word(&self) -> String {
        match self {
            State::Finished {
                outcome: RunOutcome::Failed,
                reason: Some(reason),
                ..
            } => match failure_class(reason) {
                Some(class) => format!("failed ({})", short_failure_class(class)),
                None => "failed".to_owned(),
            },
            _ => self.word(),
        }
    }

    /// The run this state is about, if any.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            State::Idle | State::Unavailable { .. } => None,
            State::Waiting { run_id, .. }
            | State::Starting { run_id, .. }
            | State::Running { run_id, .. }
            | State::Stopping { run_id, .. }
            | State::Unreadable { run_id, .. }
            | State::Finished { run_id, .. } => Some(run_id),
        }
    }

    /// Whether this state means a process is alive right now.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            State::Starting { .. } | State::Running { .. } | State::Stopping { .. }
        )
    }
}

/// The class a failure reason names, when it names one.
///
/// A model-call reason reads `the model call failed (transport): …`, so the class is
/// the word in parentheses; a missing key is named without one.
fn failure_class(reason: &str) -> Option<&str> {
    if reason.contains("model key cannot be read") {
        return Some("missing_key");
    }
    let start = reason.find("failed (")? + "failed (".len();
    let rest = &reason[start..];
    let end = rest.find(')')?;
    Some(&rest[..end])
}

/// A failure class as one whole word for the hub's narrow state column (L-6).
///
/// A class the map has not seen is returned unchanged: it is still a word, and the
/// state column's own width rule ellipsises only what genuinely cannot fit.
fn short_failure_class(class: &str) -> &str {
    match class {
        "missing_key" => "key",
        "bad_key" => "bad key",
        "malformed_reply" => "reply",
        "model_not_found" => "no model",
        "rate_limited" => "rate",
        "server_error" => "server",
        "timeout" => "timeout",
        "transport" => "transport",
        "bad_url" => "bad url",
        "missing_token" => "token",
        other => other,
    }
}

/// What happened when a start was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDisposition {
    /// The process was started.
    Started {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
    },
    /// The start is waiting for a free slot.
    Queued {
        /// The run's identity it will have.
        run_id: String,
        /// Its place in the line, starting at one.
        position: usize,
    },
    /// This agent already has a run, so nothing was started.
    Busy {
        /// The run that is already there.
        run_id: String,
    },
}

/// What happened when a stop was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDisposition {
    /// A running process was told to stop.
    Signalled,
    /// A start that had not begun was called off.
    Cancelled,
    /// There was nothing to stop.
    Nothing,
    /// The user's systemd could not be asked to stop the unit.
    Failed {
        /// Why, in a sentence.
        reason: String,
    },
}

/// What happened when one agent's supervision mode was switched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeChange {
    /// The record was written, moving the agent to this mode.
    Switched(Mode),
    /// The agent was already in the requested mode, so nothing changed.
    Already(Mode),
}

/// Why a start could not be prepared or spawned.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The agent's own directories could not be prepared.
    #[error("{0}")]
    Paths(#[from] crate::paths::PathError),
    /// The run's own directory could not be made, or its plan could not be written.
    #[error("the run {run_id} for `{agent}` cannot be prepared: {source}")]
    Prepare {
        /// The agent.
        agent: String,
        /// The run.
        run_id: String,
        /// The reason from the filesystem.
        #[source]
        source: std::io::Error,
    },
    /// The agent process could not be started.
    #[error("the agent process for `{agent}` cannot be started: {source}")]
    Spawn {
        /// The agent.
        agent: String,
        /// The reason from the operating system.
        #[source]
        source: std::io::Error,
    },
    /// The program does not know which executable to start.
    #[error("the tool's own executable cannot be found, so no agent process can be started")]
    NoBinary,
    /// The credentials file's path is not known, so a run cannot read its model key.
    #[error("the credentials file path is not known, so `{agent}` cannot read its model key")]
    Credentials {
        /// The agent.
        agent: String,
    },
    /// The user unit could not be written or started.
    #[error("`{agent}` cannot be started as a user unit: {message}")]
    Systemd {
        /// The agent.
        agent: String,
        /// Why, in a sentence from systemd or the filesystem.
        message: String,
    },
}

/// A run that is alive right now.
#[derive(Debug)]
struct Live {
    run_id: String,
    dir: PathBuf,
    child: Child,
    pid: u32,
    started: Instant,
    stopping: Option<StopKind>,
    /// When the polite signal was sent, so a forceful one can follow.
    signalled_at: Option<Instant>,
    log: fs::File,
}

/// Why a run is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    /// Someone asked for it, with `x`.
    Requested,
    /// The UI is leaving, so the runs it started end with it.
    Left,
    /// The run reached its timeout.
    TimedOut,
    /// The configuration was re-read and no longer lists this agent, so its run was
    /// stopped. This is not a stop the user asked for with `x` (T-6).
    Unlisted,
}

/// A start that is waiting for a free slot.
#[derive(Debug)]
struct Queued {
    name: String,
    paths: AgentPaths,
    model: RunModel,
    steps: Vec<String>,
    run_id: String,
    /// Which mode it will be started in when its turn comes.
    mode: Mode,
}

/// The supervisor of every agent of one installation.
#[derive(Debug)]
pub struct Supervisor {
    state_root: PathBuf,
    binary: PathBuf,
    credentials: PathBuf,
    /// The configuration directory, so an agent's supervision record can be found.
    config_root: PathBuf,
    /// The mode each agent is in, read from its record when the configuration is read.
    modes: HashMap<String, Mode>,
    /// The systemd half: units, their starts and stops, and their state.
    systemd: SystemdSupervisor,
    max_concurrent: usize,
    timeout: Duration,
    grace: Duration,
    sequence: u64,
    states: HashMap<String, State>,
    live: HashMap<String, Live>,
    queue: VecDeque<Queued>,
    /// Process groups still being swept after their run ended.
    sweeps: Vec<(u32, Instant)>,
    /// The last thing that went wrong, for the status line.
    last_error: Option<String>,
}

impl Supervisor {
    /// A supervisor over one state root, starting `binary` for each run.
    pub fn new(
        state_root: impl Into<PathBuf>,
        binary: impl Into<PathBuf>,
        max_concurrent: usize,
        timeout: Duration,
    ) -> Self {
        let unit_dir = systemd::user_unit_dir().unwrap_or_default();
        Self {
            state_root: state_root.into(),
            binary: binary.into(),
            credentials: PathBuf::new(),
            config_root: PathBuf::new(),
            modes: HashMap::new(),
            systemd: SystemdSupervisor::new(
                Systemctl::new(),
                unit_dir,
                RestartPolicy::default(),
            ),
            max_concurrent: max_concurrent.max(1),
            timeout,
            grace: DEFAULT_GRACE,
            sequence: 0,
            states: HashMap::new(),
            live: HashMap::new(),
            queue: VecDeque::new(),
            sweeps: Vec::new(),
            last_error: None,
        }
    }

    /// How long a stopped run may take to end politely.
    pub fn set_grace(&mut self, grace: Duration) {
        self.grace = grace;
    }

    /// Say where the credentials file is. A run reads its model key from there.
    pub fn set_credentials(&mut self, path: impl Into<PathBuf>) {
        self.credentials = path.into();
    }

    /// Say which configuration directory the agents live under, so their supervision
    /// records can be read and written.
    pub fn set_config_root(&mut self, path: impl Into<PathBuf>) {
        self.config_root = path.into();
    }

    /// Put the unit files somewhere else. The tests use this; production leaves the
    /// unit directory where `systemctl --user` looks for it.
    pub fn set_unit_dir(&mut self, path: impl Into<PathBuf>) {
        self.systemd.set_unit_dir(path.into());
    }

    /// Use a chosen `systemctl`, which is what the tests do.
    pub fn set_systemctl(&mut self, systemctl: Systemctl) {
        self.systemd.set_systemctl(systemctl);
    }

    /// Read every agent's supervision record, so the screens know which mode each
    /// agent is in. Called whenever the configuration is read again.
    pub fn load_modes(&mut self, agents: &[AgentPaths]) {
        self.modes.clear();
        for agent in agents {
            let record = systemd::read_record(agent);
            self.modes.insert(agent.name().to_owned(), record.mode);
            if record.mode == Mode::Systemd {
                self.systemd.note_run(
                    agent.name(),
                    record.run_dir.clone(),
                    record.restarts_recorded,
                );
            }
        }
        // An agent whose record no longer says systemd has no unit here any more, so
        // the run remembered for it is forgotten rather than counted.
        let supervised: Vec<String> = self
            .modes
            .iter()
            .filter(|(_, mode)| **mode == Mode::Systemd)
            .map(|(name, _)| name.clone())
            .collect();
        self.systemd.keep_only(&supervised);
        // Read the units back at once, so an agent whose unit is running is shown as
        // running from the first frame rather than only after the first poll.
        if !supervised.is_empty() {
            self.systemd.poll_now();
        }
    }

    /// The mode one agent is supervised in. An agent with no record is in the default
    /// mode, which is a child of this UI.
    pub fn mode(&self, name: &str) -> Mode {
        self.modes.get(name).copied().unwrap_or_default()
    }

    /// Move one agent between the two supervision modes.
    ///
    /// This is idempotent: asking for the mode an agent is already in succeeds and
    /// changes nothing. A switch is refused while the agent is running, in either
    /// mode, so that no run is ever left without something that knows about it.
    pub fn switch_mode(
        &mut self,
        agent: &AgentPaths,
        target: Mode,
    ) -> Result<ModeChange, String> {
        let name = agent.name().to_owned();
        // The record lives in the agent's own directory, so that directory is made
        // here just as it is before a start: switching mode is a change to the agent.
        agent
            .ensure()
            .map_err(|error| format!("`{name}` cannot change mode: {error}"))?;
        let current = self.mode(&name);
        if current == target {
            return Ok(ModeChange::Already(current));
        }
        match current {
            Mode::Tui => {
                let live = self.live.contains_key(&name)
                    || self
                        .states
                        .get(&name)
                        .map(|state| state.is_live())
                        .unwrap_or(false);
                if live {
                    return Err(format!(
                        "refused: `{name}` is running as a child of this UI; stop it first (press x), then switch"
                    ));
                }
            }
            Mode::Systemd => {
                self.systemd.poll_now();
                if self.systemd.is_live(&name) {
                    let unit = systemd::unit_name(&name);
                    return Err(format!(
                        "refused: `{name}` is running as the user unit {unit}; stop it first (press x), then switch"
                    ));
                }
            }
        }
        // Entering systemd mode is a promise the machine may not be able to keep. The
        // user manager is probed *before* anything is written, so a machine with no
        // user session refuses the switch outright instead of recording a mode whose
        // unit can never start — and whose way back would then need the very manager
        // that is missing (T-1). A refused switch changes nothing.
        if target == Mode::Systemd {
            if let Err(reason) = self.systemd.available() {
                return Err(format!(
                    "refused: `{name}` cannot be supervised by systemd here: {reason}. Nothing was changed: it is still supervised by this UI (tui); a systemd switch needs a reachable user manager"
                ));
            }
        }
        // Leaving systemd mode means the machine keeps no unit for this agent: a unit
        // left behind could be started by hand and would then contradict the record.
        // An agent with no unit file on disk has nothing to stop, so leaving does not
        // depend on the user manager answering (T-1).
        if target == Mode::Tui {
            self.systemd
                .remove_unit(&name)
                .map_err(|message| format!("`{name}` could not leave systemd mode: {message}"))?;
        }
        let record = match target {
            Mode::Systemd => Supervision {
                mode: Mode::Systemd,
                run_dir: None,
                restarts_recorded: 0,
            },
            Mode::Tui => Supervision::default(),
        };
        systemd::write_record(agent, &record).map_err(|error| {
            format!("the supervision record of `{name}` cannot be written: {error}")
        })?;
        self.modes.insert(name.clone(), target);
        self.states.remove(&name);
        Ok(ModeChange::Switched(target))
    }

    /// Whether the user's own systemd answers, and which version it is.
    pub fn systemd_available(&self) -> Result<String, String> {
        self.systemd.available()
    }

    /// The last problem the systemd half met, if any.
    pub fn systemd_last_error(&self) -> Option<&str> {
        self.systemd.last_error()
    }

    /// The directory unit files are written into.
    pub fn unit_dir(&self) -> &Path {
        self.systemd.unit_dir()
    }

    /// How many times systemd reports having restarted one agent's unit.
    pub fn restart_count(&self, name: &str) -> u32 {
        self.systemd.restart_count(name)
    }

    /// Apply the limits a freshly read configuration carries.
    ///
    /// The limit is read here rather than remembered per start, so raising it lets
    /// queued starts go; lowering it never stops a run that is already alive.
    pub fn set_limits(&mut self, max_concurrent: usize, timeout: Duration) {
        self.max_concurrent = max_concurrent.max(1);
        self.timeout = timeout;
    }

    /// The configured limit on agents running at once.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// The configured per-run timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How many agents are running, or starting, right now.
    pub fn running_count(&self) -> usize {
        self.live.len() + self.systemd.live_count()
    }

    /// How many starts are waiting for a free slot.
    pub fn waiting_count(&self) -> usize {
        self.queue.len()
    }

    /// What the screens should show for one agent.
    ///
    /// An agent in systemd mode is read from systemd, never from what this tool last
    /// did; an agent in the default mode is read from the process this tool holds.
    pub fn state(&self, name: &str) -> State {
        if self.mode(name) == Mode::Systemd {
            return self.systemd.state(name).cloned().unwrap_or(State::Idle);
        }
        self.states.get(name).cloned().unwrap_or(State::Idle)
    }

    /// Every agent the supervisor knows about, and its state.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut all: HashMap<String, State> = self.states.clone();
        for (name, mode) in &self.modes {
            if *mode == Mode::Systemd {
                all.insert(
                    name.clone(),
                    self.systemd.state(name).cloned().unwrap_or(State::Idle),
                );
            }
        }
        let mut all: Vec<(String, State)> = all.into_iter().collect();
        all.sort_by(|left, right| left.0.cmp(&right.0));
        all
    }

    /// The last problem, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Stop every unfinished run whose agent is not in `configured`, and return their
    /// names.
    ///
    /// The configuration file is hand-editable and `r` re-reads it. An agent removed
    /// from the file while its run is in flight would otherwise leave the run counted
    /// by the footer but invisible on the hub and unreachable by `x` — the same
    /// orphaned-run class as W-2, reached through a different door. The run is stopped
    /// here, so what the screens count is what the screens can show.
    pub fn stop_unlisted(&mut self, configured: &[String]) -> Vec<String> {
        let listed = |name: &str| configured.iter().any(|configured| configured == name);
        let mut names: Vec<String> = Vec::new();
        for name in self.live.keys().cloned().collect::<Vec<_>>() {
            if !listed(&name) {
                names.push(name);
            }
        }
        for name in self.queue.iter().map(|queued| queued.name.clone()).collect::<Vec<_>>() {
            if !listed(&name) && !names.contains(&name) {
                names.push(name);
            }
        }
        for (name, mode) in self.modes.clone() {
            if mode == Mode::Systemd && !listed(&name) && !names.contains(&name) {
                self.systemd.poll_now();
                if self.systemd.is_live(&name) {
                    names.push(name);
                }
            }
        }
        for name in &names {
            self.stop_with(name, StopKind::Unlisted);
        }
        names
    }

    /// A run of this agent that has not finished: live, stopping, or waiting for a
    /// slot.
    ///
    /// This is what removal and mode-switch guard against. `State::is_live` is not
    /// enough: a start waiting in the queue is still a run whose record was opened,
    /// and removing its agent would leave that record behind with nothing to run it.
    pub fn unfinished_run(&mut self, name: &str) -> Option<String> {
        if let Some(queued) = self.queue.iter().find(|queued| queued.name == name) {
            return Some(queued.run_id.clone());
        }
        if self.mode(name) == Mode::Systemd {
            self.systemd.poll_now();
            if self.systemd.is_live(name) {
                return self.systemd.run_of(name).map(|run| run.run_id);
            }
            return None;
        }
        self.states
            .get(name)
            .filter(|state| state.is_live())
            .and_then(|state| state.run_id())
            .map(str::to_owned)
            .or_else(|| {
                self.live
                    .get(name)
                    .map(|live| live.run_id.clone())
            })
    }

    /// Close the record of a run the UI that started it can no longer be watching.
    ///
    /// A default-mode run is a child of the UI. When that UI is gone, nothing is left
    /// to reap the child or write its ending; a reopened UI reads the run's own
    /// opening record, sees the process is dead, and closes it here as `stopped` with
    /// a reason, rather than calling the agent `idle` while the run's record says it
    /// never ended. The caller has already checked that the process is not alive; this
    /// only writes the ending and puts the state on the screens.
    pub fn note_abandoned(&mut self, name: &str, run_dir: &Path, run_id: &str) {
        let reason = "the UI that started this run is not running any more, so the run is not either"
            .to_owned();
        // The abandoned run was already going when its UI died, and the reopening UI
        // is what closes it. A duration of `0.0s` would be a lie about a run the user
        // watched for twenty-one seconds; derive it from the record's own start to
        // now, and fall back to zero only when the record carries no start at all.
        let duration_ms = transcript::first_started_at(run_dir)
            .as_deref()
            .and_then(crate::time::parse_rfc3339)
            .map(|started| crate::time::now_unix_ms().saturating_sub(started))
            .unwrap_or(0);
        let (outcome, reason, duration_ms) = record_outcome_if_missing(
            run_dir,
            run_id,
            RunOutcome::Stopped,
            Some(reason),
            duration_ms,
        );
        self.states.insert(
            name.to_owned(),
            State::Finished {
                run_id: run_id.to_owned(),
                outcome,
                reason,
                duration_ms,
            },
        );
    }

    /// Forget one agent's remembered terminal outcome.
    ///
    /// The records on disk are the truth; an outcome cached from a record that is no
    /// longer there must not outlive it on the hub, or the hub and the ledger disagree
    /// about what was recorded (T-5). The caller has already checked the record list.
    pub fn forget_state(&mut self, name: &str) {
        self.states.remove(name);
    }

    /// Put a present-but-unreadable record on the hub's state column.
    ///
    /// W41-2: a fresh start left the state `idle` (the word for no run) while the
    /// Runs screen said `unreadable`. The record is on disk; its error is known; the
    /// hub says the same word Runs does.
    pub fn note_unreadable(&mut self, name: &str, run_id: String, reason: String) {
        self.states
            .insert(name.to_owned(), State::Unreadable { run_id, reason });
    }

    /// Seed the screens with the newest terminal record of one agent.
    ///
    /// A reopened UI has no live process to watch, but the run records are on disk.
    /// The page map's "Signals and abandoned runs" row says a run left behind is shown
    /// as `stopped`, **never `idle`**; the same honesty is owed to a run that finished
    /// before the user quit. Without this, the hub forgets every outcome on every
    /// start and the reason a run ended is gone from the screen the user lands on.
    pub fn note_last_outcome(
        &mut self,
        name: &str,
        run_id: &str,
        outcome: RunOutcome,
        reason: Option<String>,
        duration_ms: u64,
    ) {
        self.states.insert(
            name.to_owned(),
            State::Finished {
                run_id: run_id.to_owned(),
                outcome,
                reason,
                duration_ms,
            },
        );
    }

    /// Ask for one run of one agent.
    ///
    /// Beyond the concurrency limit the start does not fail: it waits its turn, and
    /// the poll that follows starts it when a slot is free. Which mode the agent is
    /// in decides who starts the process, never the caller.
    pub fn start(
        &mut self,
        agent: &AgentPaths,
        model: &RunModel,
        steps: Vec<String>,
    ) -> Result<StartDisposition, StartError> {
        let name = agent.name().to_owned();
        let mode = self.mode(&name);
        let live_run = match mode {
            Mode::Tui => self
                .states
                .get(&name)
                .filter(|state| state.is_live())
                .and_then(|state| state.run_id())
                .map(str::to_owned),
            Mode::Systemd => {
                self.systemd.poll_now();
                if self.systemd.is_live(&name) {
                    self.systemd.run_of(&name).map(|run| run.run_id)
                } else {
                    None
                }
            }
        };
        if let Some(run_id) = live_run {
            return Ok(StartDisposition::Busy { run_id });
        }
        if self.queue.iter().any(|queued| queued.name == name) {
            let run_id = self
                .queue
                .iter()
                .find(|queued| queued.name == name)
                .map(|queued| queued.run_id.clone())
                .unwrap_or_default();
            return Ok(StartDisposition::Busy { run_id });
        }
        if self.binary.as_os_str().is_empty() {
            return Err(StartError::NoBinary);
        }

        let run_id = self.next_run_id();
        if self.running_count() >= self.max_concurrent {
            self.queue.push_back(Queued {
                name: name.clone(),
                paths: agent.clone(),
                model: model.clone(),
                steps,
                run_id: run_id.clone(),
                mode,
            });
            let position = self.queue.len();
            self.states.insert(
                name,
                State::Waiting {
                    run_id: run_id.clone(),
                    position,
                },
            );
            self.reindex_queue();
            return Ok(StartDisposition::Queued { run_id, position });
        }
        let pid = match mode {
            Mode::Tui => self.spawn(agent, model, steps, run_id.clone())?,
            Mode::Systemd => self.spawn_systemd(agent, model, steps, run_id.clone())?,
        };
        Ok(StartDisposition::Started { run_id, pid })
    }

    /// Ask one agent to stop. A start still waiting is called off.
    ///
    /// In the default mode a stop signals the whole process group; in systemd mode it
    /// asks the user's own `systemctl --user` to stop the unit, and records that the
    /// run was stopped.
    pub fn stop(&mut self, name: &str) -> StopDisposition {
        self.stop_with(name, StopKind::Requested)
    }

    /// Stop one agent, recording *why* in the run's own log: a stop asked for with
    /// `x`, a configuration re-read that no longer lists the agent, or the UI leaving.
    fn stop_with(&mut self, name: &str, kind: StopKind) -> StopDisposition {
        if let Some(queued_at) = self.queue.iter().position(|queued| queued.name == name) {
            let queued = self
                .queue
                .remove(queued_at)
                .expect("the position was just found");
            let reason = match kind {
                StopKind::Unlisted => {
                    "the configuration no longer lists this agent, so its start was called off"
                        .to_owned()
                }
                _ => "the start was called off before a slot was free".to_owned(),
            };
            self.states.insert(
                name.to_owned(),
                State::Finished {
                    run_id: queued.run_id,
                    outcome: RunOutcome::Stopped,
                    reason: Some(reason),
                    duration_ms: 0,
                },
            );
            self.reindex_queue();
            return StopDisposition::Cancelled;
        }
        if self.mode(name) == Mode::Systemd {
            let reason = match kind {
                StopKind::Unlisted => "the user unit was stopped because the configuration no longer lists this agent",
                _ => "the user unit was stopped on request",
            };
            return match self.systemd.stop_with_reason(name, reason) {
                Ok(true) => StopDisposition::Signalled,
                Ok(false) => StopDisposition::Nothing,
                Err(reason) => StopDisposition::Failed { reason },
            };
        }
        let Some(live) = self.live.get_mut(name) else {
            return StopDisposition::Nothing;
        };
        live.stopping = Some(kind);
        live.signalled_at = Some(Instant::now());
        let (pid, run_id) = (live.pid, live.run_id.clone());
        signal_group(pid, Signal::SIGTERM);
        write_log_line(
            &mut live.log,
            &format!("supervisor: a stop was asked for, SIGTERM sent to process group {pid}"),
        );
        let elapsed_ms = live.started.elapsed().as_millis() as u64;
        self.states.insert(
            name.to_owned(),
            State::Stopping {
                run_id,
                pid,
                elapsed_ms,
            },
        );
        StopDisposition::Signalled
    }

    /// Stop every run of the default mode and wait, within a budget, until no child is
    /// left.
    ///
    /// This is what the tool calls before it leaves. It returns how many runs it
    /// stopped. It proves the children are gone by reaping them. An agent in systemd
    /// mode is deliberately left alone: surviving this UI exiting is the whole reason
    /// that mode exists, so it is not stopped here.
    pub fn stop_all(&mut self, budget: Duration) -> usize {
        let stopped = self.live.len();
        let names: Vec<String> = self.live.keys().cloned().collect();
        for name in &names {
            if let Some(live) = self.live.get_mut(name) {
                live.stopping = Some(StopKind::Left);
                live.signalled_at = Some(Instant::now());
                signal_group(live.pid, Signal::SIGTERM);
                let (pid, run_id, elapsed_ms) = (
                    live.pid,
                    live.run_id.clone(),
                    live.started.elapsed().as_millis() as u64,
                );
                self.states.insert(
                    name.clone(),
                    State::Stopping {
                        run_id,
                        pid,
                        elapsed_ms,
                    },
                );
            }
        }
        // Cancel everything still waiting.
        let waiting: Vec<String> = self
            .queue
            .iter()
            .map(|queued| queued.name.clone())
            .collect();
        for name in waiting {
            self.stop(&name);
        }

        let deadline = Instant::now() + budget;
        while !self.live.is_empty() && Instant::now() < deadline {
            self.poll(Instant::now());
            if !self.live.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Anything still alive after the budget is ended forcefully.
        let remaining: Vec<String> = self.live.keys().cloned().collect();
        for name in remaining {
            if let Some(live) = self.live.get_mut(&name) {
                signal_group(live.pid, Signal::SIGKILL);
                let _ = live.child.wait();
                let (run_id, duration_ms) = (
                    live.run_id.clone(),
                    live.started.elapsed().as_millis() as u64,
                );
                self.states.insert(
                    name,
                    State::Finished {
                        run_id,
                        outcome: RunOutcome::Stopped,
                        reason: Some("the tool left before the run ended".to_owned()),
                        duration_ms,
                    },
                );
            }
        }
        self.live.clear();
        stopped
    }

    /// Read every live process once: start what can start, finish what has finished,
    /// and stop what has overrun. The user's systemd is read on its own, slower
    /// cadence, so a unit's state is fresh without a call to systemd per frame.
    pub fn poll(&mut self, now: Instant) {
        self.systemd.poll(now);
        self.poll_untracked_systemd();
        self.sweep_groups(now);

        let names: Vec<String> = self.live.keys().cloned().collect();
        for name in names {
            self.poll_one(&name, now);
        }

        // Fill free slots from the line, in turn, in the mode each start was queued in.
        while self.running_count() < self.max_concurrent {
            let Some(queued) = self.queue.pop_front() else {
                break;
            };
            let run_id = queued.run_id.clone();
            let started = match queued.mode {
                Mode::Tui => self
                    .spawn(&queued.paths, &queued.model, queued.steps, run_id.clone())
                    .map(|_| ()),
                Mode::Systemd => self
                    .spawn_systemd(&queued.paths, &queued.model, queued.steps, run_id.clone())
                    .map(|_| ()),
            };
            if let Err(error) = started {
                self.last_error = Some(error.to_string());
                self.states.insert(
                    queued.name.clone(),
                    State::Finished {
                        run_id: queued.run_id,
                        outcome: RunOutcome::Failed,
                        reason: Some(error.to_string()),
                        duration_ms: 0,
                    },
                );
            }
        }
        self.reindex_queue();
    }

    /// A `systemd` record with no run has no unit to read, so `poll_now` would never
    /// ask the manager about it and a machine whose manager has gone would show
    /// `systemd` + `idle` with nothing warning the user (M-6). Probe the manager for
    /// exactly that case — only when no unit of this agent is being read anyway, so a
    /// tracked unit's own error is never overwritten.
    fn poll_untracked_systemd(&mut self) {
        let mut systemd_record = false;
        let mut unit_tracked = false;
        for (name, mode) in &self.modes {
            if *mode == Mode::Systemd {
                systemd_record = true;
                if self.systemd.state(name).is_some() {
                    unit_tracked = true;
                }
            }
        }
        if !systemd_record || unit_tracked {
            return;
        }
        match self.systemd.available() {
            Ok(_) => self.systemd.set_manager_error(None),
            Err(message) => {
                // `available` prefixes its sentence already; the screen adds that
                // prefix of its own, so keep only the detail here.
                let detail = message
                    .strip_prefix("the user's systemd cannot be reached: ")
                    .unwrap_or(&message)
                    .to_owned();
                self.systemd.set_manager_error(Some(detail));
            }
        }
    }

    /// Read one live process.
    fn poll_one(&mut self, name: &str, now: Instant) {
        let Some(live) = self.live.get_mut(name) else {
            return;
        };
        let elapsed_ms = live.started.elapsed().as_millis() as u64;

        // A run that has overrun its timeout is stopped for that reason.
        if live.stopping.is_none() && live.started.elapsed() >= self.timeout {
            live.stopping = Some(StopKind::TimedOut);
            live.signalled_at = Some(now);
            let pid = live.pid;
            signal_group(pid, Signal::SIGTERM);
            write_log_line(
                &mut live.log,
                &format!(
                    "supervisor: the run reached its timeout of {} s, SIGTERM sent to process group {pid}",
                    self.timeout.as_secs()
                ),
            );
            self.states.insert(
                name.to_owned(),
                State::Stopping {
                    run_id: live.run_id.clone(),
                    pid,
                    elapsed_ms,
                },
            );
        }

        // End a run that will not end politely, once its grace has passed.
        if let (Some(_), Some(signalled_at)) = (live.stopping, live.signalled_at) {
            if now.duration_since(signalled_at) >= self.grace {
                let pid = live.pid;
                signal_group(pid, Signal::SIGKILL);
                live.signalled_at = Some(now);
                write_log_line(
                    &mut live.log,
                    &format!(
                        "supervisor: the run did not end within its grace, SIGKILL sent to process group {pid}"
                    ),
                );
            }
        }

        match live.child.try_wait() {
            Ok(Some(status)) => self.finish(name, status),
            Ok(None) => {
                // Seen alive: from here on the screens say running.
                let state = State::Running {
                    run_id: live.run_id.clone(),
                    pid: live.pid,
                    elapsed_ms,
                };
                if live.stopping.is_none() {
                    self.states.insert(name.to_owned(), state);
                }
            }
            Err(error) => {
                self.last_error = Some(format!("the agent process cannot be read: {error}"));
                let run_id = live.run_id.clone();
                let pid = live.pid;
                let duration_ms = live.started.elapsed().as_millis() as u64;
                signal_group(pid, Signal::SIGTERM);
                let _ = live.child.wait();
                if group_has_members(pid) {
                    self.sweeps.push((pid, Instant::now() + self.grace));
                }
                let dir = live.dir.clone();
                self.live.remove(name);
                self.record_outcome(
                    name,
                    &dir,
                    &run_id,
                    RunOutcome::Failed,
                    Some(format!("the agent process cannot be read: {error}")),
                    duration_ms,
                );
            }
        }
    }

    /// Finish one run whose process has exited.
    fn finish(&mut self, name: &str, status: std::process::ExitStatus) {
        let Some(mut live) = self.live.remove(name) else {
            return;
        };
        // Signal the group while the child is still an unreaped member, so the group
        // id cannot have been given to anyone else yet, then reap the child.
        let pid = live.pid;
        signal_group(pid, Signal::SIGTERM);
        let _ = live.child.wait();
        let duration_ms = live.started.elapsed().as_millis() as u64;
        // Only a group that still has members needs to be swept again, forcefully.
        if group_has_members(pid) {
            self.sweeps.push((pid, Instant::now() + self.grace));
        }

        // The process decides. A stop the tool asked for is `stopped`; a stop for the
        // timeout is `timed_out`; otherwise the exit code decides.
        let (outcome, reason) = match live.stopping {
            Some(StopKind::Requested) => (
                RunOutcome::Stopped,
                Some("the run was stopped on request".to_owned()),
            ),
            Some(StopKind::Left) => (
                RunOutcome::Stopped,
                Some("the UI was closed, so the run it started was stopped".to_owned()),
            ),
            Some(StopKind::TimedOut) => (
                RunOutcome::TimedOut,
                Some(format!(
                    "the run reached its timeout of {} s and was stopped",
                    self.timeout.as_secs()
                )),
            ),
            Some(StopKind::Unlisted) => (
                RunOutcome::Stopped,
                Some(
                    "the configuration no longer lists this agent, so its run was stopped"
                        .to_owned(),
                ),
            ),
            None => {
                if status.success() {
                    (RunOutcome::Succeeded, None)
                } else {
                    (
                        RunOutcome::Failed,
                        Some(describe_status(&status, &live.run_id)),
                    )
                }
            }
        };
        write_log_line(
            &mut live.log,
            &format!(
                "supervisor: run {} is over: {}",
                live.run_id,
                match &reason {
                    Some(reason) => format!("{} ({reason})", outcome.as_str()),
                    None => outcome.as_str().to_owned(),
                }
            ),
        );
        let run_id = live.run_id.clone();
        let dir = live.dir.clone();
        self.record_outcome(name, &dir, &run_id, outcome, reason, duration_ms);
    }

    /// Record one terminal outcome, unless the run already wrote its own.
    fn record_outcome(
        &mut self,
        name: &str,
        run_dir: &Path,
        run_id: &str,
        outcome: RunOutcome,
        reason: Option<String>,
        duration_ms: u64,
    ) {
        let (outcome, reason, duration_ms) = record_outcome_if_missing(
            run_dir,
            run_id,
            outcome,
            reason,
            duration_ms,
        );
        self.states.insert(
            name.to_owned(),
            State::Finished {
                run_id: run_id.to_owned(),
                outcome,
                reason,
                duration_ms,
            },
        );
    }

    /// Prepare one run's directory, for either mode.
    ///
    /// Both modes do exactly this: create the run's directory, write its plan, open
    /// its log and stamp the line that says a run is starting. The only difference
    /// between the modes is who starts the process afterwards, which is why the two
    /// leave the same kind of record.
    fn prepare_run(
        &self,
        agent: &AgentPaths,
        model: &RunModel,
        steps: Vec<String>,
        run_id: &str,
    ) -> Result<(PathBuf, fs::File), StartError> {
        let name = agent.name().to_owned();
        if self.credentials.as_os_str().is_empty() {
            return Err(StartError::Credentials { agent: name });
        }
        agent.ensure()?;
        let run_dir = agent.ensure_run_dir(run_id).map_err(|error| StartError::Prepare {
            agent: name.clone(),
            run_id: run_id.to_owned(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let plan = RunPlan {
            run_id: run_id.to_owned(),
            agent: name.clone(),
            workspace: agent.workspace().to_path_buf(),
            instruction: agent.instruction().to_path_buf(),
            skills_dir: agent.skills_dir().to_path_buf(),
            endpoint: model.endpoint.clone(),
            model: model.model.clone(),
            credentials: self.credentials.clone(),
            max_turns: model.max_turns,
            steps,
            timeout_secs: self.timeout.as_secs(),
        };
        plan.write(&run_dir).map_err(|source| StartError::Prepare {
            agent: name.clone(),
            run_id: run_id.to_owned(),
            source,
        })?;

        let log_path = run_dir.join(LOG_FILE);
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(&log_path)
            .map_err(|source| StartError::Prepare {
                agent: name.clone(),
                run_id: run_id.to_owned(),
                source,
            })?;
        write_log_line(
            &mut log,
            &format!(
                "supervisor: starting run {run_id} of `{name}` in {}",
                agent.workspace().display()
            ),
        );
        Ok((run_dir, log))
    }

    /// Start the child process of one run of the default mode.
    fn spawn(
        &mut self,
        agent: &AgentPaths,
        model: &RunModel,
        steps: Vec<String>,
        run_id: String,
    ) -> Result<u32, StartError> {
        let name = agent.name().to_owned();
        let (run_dir, log) = self.prepare_run(agent, model, steps, &run_id)?;

        let mut command = Command::new(&self.binary);
        command
            .arg("--run-agent")
            .arg(&run_dir)
            .current_dir(agent.workspace())
            // The agent process writes its own log file, so its own streams are set
            // aside: nothing an agent prints can reach the terminal UI's screen.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A new process group, so one signal reaches the agent and everything it
            // started. The child becomes the leader, so its group id is its own id.
            .process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                // The run directory and plan were opened before the process was; a
                // start that never happened must not be left as a run with no ending
                // (T-8). Close it as failed, with the reason the caller will show.
                record_outcome_if_missing(
                    &run_dir,
                    &run_id,
                    RunOutcome::Failed,
                    Some(format!("the run could not be started: {source}")),
                    0,
                );
                return Err(StartError::Spawn {
                    agent: name.clone(),
                    source,
                });
            }
        };
        let pid = child.id();
        self.live.insert(
            name.clone(),
            Live {
                run_id: run_id.clone(),
                dir: run_dir,
                child,
                pid,
                started: Instant::now(),
                stopping: None,
                signalled_at: None,
                log,
            },
        );
        self.states.insert(name, State::Starting { run_id, pid });
        Ok(pid)
    }

    /// Start one run as a user unit of the user's own systemd.
    ///
    /// The run's directory and plan are prepared exactly as the default mode prepares
    /// them, and the unit runs exactly the command the UI would run, so the two modes
    /// leave the same log and transcript. The unit is written for this one run: a
    /// restart by systemd re-runs the same command against the same run, which is why
    /// a restart is visible in the run's own record.
    fn spawn_systemd(
        &mut self,
        agent: &AgentPaths,
        model: &RunModel,
        steps: Vec<String>,
        run_id: String,
    ) -> Result<u32, StartError> {
        let name = agent.name().to_owned();
        let (run_dir, _log) = self.prepare_run(agent, model, steps, &run_id)?;
        let spec = UnitSpec::for_run(
            &name,
            &self.binary,
            &run_dir,
            agent.workspace(),
            &run_dir.join(LOG_FILE),
            RestartPolicy::default(),
        );
        let pid = match self.systemd.start(&spec, run_id.clone(), run_dir.clone()) {
            Ok(pid) => pid,
            Err(message) => {
                // systemd refused the start, so no run ever ran. The directory opened
                // for it is closed as failed here, with the reason, rather than left
                // for ever as `unknown start / no terminal record` (T-8).
                record_outcome_if_missing(
                    &run_dir,
                    &run_id,
                    RunOutcome::Failed,
                    Some(format!("the start failed: {message}")),
                    0,
                );
                return Err(StartError::Systemd {
                    agent: name.clone(),
                    message,
                });
            }
        };
        // Record which run the unit points at, so a later session reads the unit's
        // state rather than the tool's memory. A failure to write the record after the
        // unit has started is reported, but it is not a reason to claim the start did
        // not happen: it did, and systemd has it.
        let record = Supervision {
            mode: Mode::Systemd,
            run_dir: Some(run_dir),
            restarts_recorded: 0,
        };
        if let Err(error) = systemd::write_record(agent, &record) {
            self.last_error = Some(format!(
                "the supervision record of `{name}` cannot be written: {error}"
            ));
        }
        Ok(pid)
    }

    /// Escalate the sweep of process groups whose run has ended.
    fn sweep_groups(&mut self, now: Instant) {
        let mut remaining = Vec::new();
        for (pid, deadline) in self.sweeps.drain(..) {
            // A group that is already empty is left alone: a process group id that has
            // been given back must never be signalled, because it may name someone else
            // by now.
            if !group_has_members(pid) {
                continue;
            }
            if now >= deadline {
                signal_group(pid, Signal::SIGKILL);
            } else {
                remaining.push((pid, deadline));
            }
        }
        self.sweeps = remaining;
    }

    /// Number the line again after it changes.
    fn reindex_queue(&mut self) {
        for (at, queued) in self.queue.iter().enumerate() {
            self.states.insert(
                queued.name.clone(),
                State::Waiting {
                    run_id: queued.run_id.clone(),
                    position: at + 1,
                },
            );
        }
    }

    /// The next run id this supervisor will hand out.
    fn next_run_id(&mut self) -> String {
        self.sequence += 1;
        new_run_id(self.sequence)
    }

    /// The directory of a run, for a caller that has only the id.
    pub fn run_dir(&self, name: &str, run_id: &str) -> PathBuf {
        self.state_root.join(name).join(run_id)
    }
}

/// Write a run's terminal record, unless the run wrote one itself, and hand back the
/// outcome the transcript actually holds.
///
/// A run that was killed before it could write its own ending — a child stopped by
/// the UI, a unit stopped with `systemctl --user stop` — gets one written for it, so
/// every run has exactly one ending decided by something that saw it. A run that
/// wrote its own ending keeps it: the supervisor's opinion never overwrites the
/// process's own record.
pub fn record_outcome_if_missing(
    run_dir: &Path,
    run_id: &str,
    outcome: RunOutcome,
    reason: Option<String>,
    duration_ms: u64,
) -> (RunOutcome, Option<String>, u64) {
    let existing = transcript::terminal_record(run_dir);
    if existing.is_none() {
        if let Ok(mut transcript) = Transcript::open(run_dir) {
            let _ = transcript.append(&Record::run_finished(
                run_id,
                outcome,
                None,
                duration_ms,
                reason.clone(),
            ));
        }
    }
    match existing {
        Some(Record::RunFinished {
            outcome,
            reason,
            duration_ms,
            ..
        }) => (
            RunOutcome::parse(&outcome).unwrap_or(RunOutcome::Failed),
            reason,
            duration_ms,
        ),
        _ => (outcome, reason, duration_ms),
    }
}

/// One line for the log.
fn write_log_line(log: &mut fs::File, line: &str) {
    let _ = writeln!(log, "{} {line}", crate::run::started_stamp());
    let _ = log.flush();
}

/// Describe an exit status the tool did not ask for.
fn describe_status(status: &std::process::ExitStatus, run_id: &str) -> String {
    match status.code() {
        Some(code) => format!("run {run_id} exited with code {code}"),
        None => match signal_of(*status) {
            Some(signal) => format!("run {run_id} was ended by signal {signal}"),
            None => format!("run {run_id} ended with no status"),
        },
    }
}

/// The signal that ended a process, when one did.
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Send a signal to a whole process group. A group that is already gone is not an
/// error: the thing this function exists for has already happened.
fn signal_group(pid: u32, signal: Signal) {
    let _ = killpg(Pid::from_raw(pid as i32), signal);
}

/// Whether a process with this id is still alive.
///
/// A process owned by another user is still alive; only a process that is gone counts
/// as dead. This is what lets a reopened UI tell a run still in progress from one left
/// behind by a UI that is no longer there.
pub fn process_alive(pid: u32) -> bool {
    match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Whether any process on this machine is still in the process group `pgid`.
///
/// This is read from Linux's own `/proc`, so it is the operating system's answer, not
/// a guess. Where `/proc` cannot be read, the answer is `true` — the careful direction,
/// because it keeps a run's cleanup going rather than declaring it done.
fn group_has_members(pgid: u32) -> bool {
    let entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The shape is `pid (name) state ppid pgrp ...`, and the name may hold
        // spaces or brackets, so the fields are read after the last `)`.
        let Some((_, after_name)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = after_name.split_whitespace();
        let _state = fields.next();
        let _parent = fields.next();
        let pgrp = fields.next().and_then(|value| value.parse::<u32>().ok());
        if pgrp == Some(pgid) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::PLAN_FILE;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_hub_and_the_runs_screen_use_one_word_for_success() {
        // W-12: one event, one word. The Runs screen says `succeeded`; the hub used
        // to say `exited 0` for the same ending.
        let state = State::Finished {
            run_id: "run-1".to_owned(),
            outcome: RunOutcome::Succeeded,
            reason: None,
            duration_ms: 25_000,
        };
        assert_eq!(state.word(), "succeeded");
        assert_eq!(state.word_with_reason(), "succeeded");
    }

    #[test]
    fn a_failure_class_is_a_whole_word_on_the_hub() {
        // L-6: `failed (server_error)` was ellipsised to `failed (s…er_error)` on the
        // hub — not a word. Every class the model and Forgejo clients produce now
        // shortens to a whole word, and the column width accommodates them.
        let cases = [
            ("the model call failed (server_error): the endpoint answered 500", "failed (server)"),
            ("the model call failed (malformed_reply): no choices", "failed (reply)"),
            ("the model call failed (model_not_found): no such model", "failed (no model)"),
            ("the model call failed (rate_limited): slow down", "failed (rate)"),
            ("the model call failed (transport): the connection dropped", "failed (transport)"),
            ("the model call failed (timeout): too slow", "failed (timeout)"),
            ("the model call failed (bad_key): the endpoint refused it", "failed (bad key)"),
        ];
        for (reason, expected) in cases {
            let state = State::Finished {
                run_id: "run-1".to_owned(),
                outcome: RunOutcome::Failed,
                reason: Some(reason.to_owned()),
                duration_ms: 1,
            };
            assert_eq!(state.cell_word(), expected, "{reason}");
            assert!(
                !state.cell_word().contains('…'),
                "a class was ellipsised: {}",
                state.cell_word()
            );
        }
    }

    #[test]
    fn a_run_ended_by_leaving_names_the_ui_closing() {
        // W-10: the record of a run stopped by `q` must not read "on request", as if
        // the user had pressed `x`.
        let binary = fake_agent("sup-left", "sleep 30");
        let (layout, agent) = layout("sup-left");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("the run starts");
        wait_for("the run to be live", || {
            supervisor.state("scribe").is_live()
        });
        supervisor.stop_all(Duration::from_secs(3));
        match supervisor.state("scribe") {
            State::Finished { reason, .. } => {
                let reason = reason.expect("a reason is recorded");
                assert!(reason.contains("UI was closed"), "{reason}");
            }
            other => panic!("expected a finished state, got {other:?}"),
        }
    }

    #[test]
    fn an_unfinished_run_is_reported_for_removal_and_switch_guards() {
        // W-2's guard reads this: a waiting start is unfinished too, so removing its
        // agent could not leave the record behind.
        let binary = fake_agent("sup-unfinished", "sleep 30");
        let (layout, agent) = layout("sup-unfinished");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        let first = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("the first start");
        assert!(matches!(first, StartDisposition::Started { .. }));
        wait_for("the run to be known", || {
            supervisor.unfinished_run("scribe").is_some()
        });
        supervisor.stop_all(Duration::from_secs(2));
        assert!(supervisor.unfinished_run("scribe").is_none());
    }

    /// Write a fake agent process and return its path.
    ///
    /// It is a shell script, not the real binary, because these tests are about the
    /// supervisor's own rules — the limit, the line, the timeout, the signals — and a
    /// script can be told to do exactly one of those things.
    fn fake_agent(tag: &str, body: &str) -> PathBuf {
        let path = scratch_root(tag).join("fake-agent.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the fake is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the fake is runnable");
        path
    }

    fn layout(tag: &str) -> (crate::paths::Layout, AgentPaths) {
        let root = scratch_root(tag);
        let layout = crate::paths::Layout::rooted_at(&root);
        let agent = layout
            .ensure_agent("scribe")
            .expect("the agent is prepared");
        (layout, agent)
    }

    /// A supervisor with a credentials file already in place, so a start can prepare
    /// a plan. The fake agent ignores the plan; these tests are about the supervisor.
    fn supervisor(
        layout: &crate::paths::Layout,
        binary: PathBuf,
        max_concurrent: usize,
        timeout: Duration,
    ) -> Supervisor {
        let credentials = layout.root().join("credentials.toml");
        crate::mode::write_private(
            &credentials,
            b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
        )
        .expect("the credentials file is written");
        let mut supervisor = Supervisor::new(layout.state_root(), binary, max_concurrent, timeout);
        supervisor.set_credentials(credentials);
        supervisor
    }

    fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_started_run_is_visible_and_then_finishes_cleanly() {
        let binary = fake_agent("sup-ok", "exit 0");
        let (layout, agent) = layout("sup-ok");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        let disposition = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        assert!(matches!(disposition, StartDisposition::Started { .. }));
        assert_eq!(supervisor.running_count(), 1);
        wait_for("the run to finish", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Succeeded,
                    ..
                }
            )
        });
        assert_eq!(supervisor.running_count(), 0);
    }

    #[test]
    fn a_non_zero_exit_is_a_failure_with_a_reason() {
        let binary = fake_agent("sup-fail", "exit 3");
        let (layout, agent) = layout("sup-fail");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        wait_for("the failure to be recorded", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Failed,
                    ..
                }
            )
        });
        match supervisor.state("scribe") {
            State::Finished { reason, .. } => {
                assert!(reason.expect("a reason").contains("code 3"));
            }
            other => panic!("unexpected state {other:?}"),
        }
    }

    #[test]
    fn a_start_beyond_the_limit_waits_its_turn_and_then_runs() {
        let binary = fake_agent("sup-limit", "sleep 30");
        let (layout, _) = layout("sup-limit");
        let one = layout.ensure_agent("one").expect("one");
        let two = layout.ensure_agent("two").expect("two");
        let mut supervisor = supervisor(&layout, binary, 1, Duration::from_secs(30));

        let first = supervisor
            .start(&one, &RunModel::default(), Vec::new())
            .expect("started");
        assert!(matches!(first, StartDisposition::Started { .. }));
        let second = supervisor
            .start(&two, &RunModel::default(), Vec::new())
            .expect("queued");
        match second {
            StartDisposition::Queued { position, .. } => assert_eq!(position, 1),
            other => panic!("expected a wait, got {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 1);
        assert_eq!(supervisor.waiting_count(), 1);
        assert!(matches!(
            supervisor.state("two"),
            State::Waiting { position: 1, .. }
        ));

        // With the slot free, the poll starts the start that was waiting.
        supervisor.stop("one");
        wait_for("the waiting start to begin", || {
            supervisor.poll(Instant::now());
            match supervisor.state("two") {
                State::Starting { .. } | State::Running { .. } => true,
                State::Finished { .. } => true,
                State::Waiting { .. } => false,
                State::Idle
                | State::Stopping { .. }
                | State::Unavailable { .. }
                | State::Unreadable { .. } => false,
            }
        });
        assert_eq!(supervisor.running_count(), 1);
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn calling_off_a_waiting_start_leaves_a_stopped_run_behind() {
        let binary = fake_agent("sup-cancel", "sleep 30");
        let (layout, _) = layout("sup-cancel");
        let one = layout.ensure_agent("one").expect("one");
        let two = layout.ensure_agent("two").expect("two");
        let mut supervisor = supervisor(&layout, binary, 1, Duration::from_secs(30));
        supervisor
            .start(&one, &RunModel::default(), Vec::new())
            .expect("started");
        supervisor
            .start(&two, &RunModel::default(), Vec::new())
            .expect("queued");
        assert_eq!(supervisor.stop("two"), StopDisposition::Cancelled);
        assert_eq!(supervisor.waiting_count(), 0);
        assert!(matches!(
            supervisor.state("two"),
            State::Finished {
                outcome: RunOutcome::Stopped,
                ..
            }
        ));
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn a_run_that_overruns_its_timeout_is_stopped_and_reported() {
        let binary = fake_agent("sup-timeout", "sleep 30");
        let (layout, agent) = layout("sup-timeout");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_millis(200));
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        wait_for("the timeout", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::TimedOut,
                    ..
                }
            )
        });
        match supervisor.state("scribe") {
            State::Finished { reason, .. } => {
                assert!(reason.expect("a reason").contains("timeout"));
            }
            other => panic!("unexpected state {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 0);
    }

    #[test]
    fn stopping_a_run_is_reported_as_stopped_and_the_process_is_gone() {
        let binary = fake_agent("sup-stop", "sleep 30");
        let (layout, agent) = layout("sup-stop");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        let disposition = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let pid = match disposition {
            StartDisposition::Started { pid, .. } => pid,
            other => panic!("unexpected {other:?}"),
        };
        wait_for("the process to be alive", || {
            supervisor.poll(Instant::now());
            matches!(supervisor.state("scribe"), State::Running { .. })
        });
        assert_eq!(supervisor.stop("scribe"), StopDisposition::Signalled);
        wait_for("the stop to be recorded", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Stopped,
                    ..
                }
            )
        });
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the child process must be gone"
        );
    }

    #[test]
    fn a_busy_agent_is_not_started_twice() {
        let binary = fake_agent("sup-busy", "sleep 30");
        let (layout, agent) = layout("sup-busy");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        assert!(matches!(
            supervisor
                .start(&agent, &RunModel::default(), Vec::new())
                .expect("refused"),
            StartDisposition::Busy { .. }
        ));
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn the_terminal_record_of_a_stopped_run_is_written_once() {
        let binary = fake_agent("sup-record", "sleep 30");
        let (layout, agent) = layout("sup-record");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        let disposition = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let run_id = match disposition {
            StartDisposition::Started { run_id, .. } => run_id,
            other => panic!("unexpected {other:?}"),
        };
        wait_for("running", || {
            supervisor.poll(Instant::now());
            matches!(supervisor.state("scribe"), State::Running { .. })
        });
        supervisor.stop("scribe");
        wait_for("stopped", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Stopped,
                    ..
                }
            )
        });
        let run_dir = supervisor.run_dir("scribe", &run_id);
        let file = transcript::read(&run_dir).expect("the transcript reads");
        let terminals = file
            .records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .count();
        assert_eq!(terminals, 1, "exactly one terminal record");
        assert!(run_dir.join(PLAN_FILE).is_file());
        assert!(run_dir.join(LOG_FILE).is_file());
    }

    #[test]
    fn stop_all_changes_nothing_when_nothing_runs() {
        let binary = fake_agent("sup-empty", "exit 0");
        let (layout, _) = layout("sup-empty");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        assert_eq!(supervisor.stop_all(Duration::from_millis(100)), 0);
    }

    /// The process group of this very process, read from `/proc/self/stat`.
    fn own_process_group() -> u32 {
        let stat = fs::read_to_string("/proc/self/stat").expect("this process is on Linux");
        let (_, after_name) = stat.rsplit_once(')').expect("stat holds a name");
        let mut fields = after_name.split_whitespace();
        let _state = fields.next();
        let _parent = fields.next();
        fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .expect("the group is a number")
    }

    #[test]
    fn a_process_group_with_members_is_told_apart_from_an_empty_one() {
        let own = own_process_group();
        assert!(group_has_members(own), "this process is in group {own}");
        assert!(
            !group_has_members(3_999_999_999),
            "a group id nothing can be using is empty"
        );
    }

    /// A stand-in for `systemctl --user`, with the files that drive it.
    ///
    /// These tests are about the commands the tool builds and the states it reads
    /// back. A script that records its arguments and reports a state the test chooses
    /// exercises exactly the client the real unit test uses, without a real unit; the
    /// real thing is exercised by `tests/systemd.rs`.
    struct FakeSystemd {
        program: PathBuf,
        state: PathBuf,
        restarts: PathBuf,
        commands: PathBuf,
    }

    const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
STATE="__STATE__"
RESTARTS="__RESTARTS__"
LOG="__LOG__"
printf '%s\n' "$*" >> "$LOG"
[ "$1" = "--user" ] && shift
verb="$1"
shift
unit=""
for a in "$@"; do
    case "$a" in
        -*) ;;
        *) if [ -z "$unit" ]; then unit="$a"; fi ;;
    esac
done
case "$verb" in
    daemon-reload|enable|disable) exit 0 ;;
    start) printf '%s' active > "$STATE"; exit 0 ;;
    stop) printf '%s' inactive > "$STATE"; exit 0 ;;
    show)
        if [ -z "$unit" ]; then printf '%s\n' "255.0-fake"; exit 0; fi
        state="$(cat "$STATE" 2>/dev/null || printf '%s' inactive)"
        printf 'LoadState=loaded\nNRestarts=%s\n' "$(cat "$RESTARTS" 2>/dev/null || printf '0')"
        case "$state" in
            active) printf 'ActiveState=active\nSubState=running\nMainPID=4242\nResult=success\nActiveEnterTimestampMonotonic=1\n' ;;
            failed) printf 'ActiveState=failed\nSubState=failed\nMainPID=0\nResult=exit-code\nExecMainStatus=1\n' ;;
            notfound) printf 'LoadState=not-found\nActiveState=inactive\nSubState=dead\nMainPID=0\nResult=success\n' ;;
            *) printf 'ActiveState=inactive\nSubState=dead\nMainPID=0\nResult=success\n' ;;
        esac
        ;;
    *) exit 0 ;;
esac
"#;

    fn fake_systemctl(tag: &str) -> FakeSystemd {
        let root = scratch_root(tag);
        let state = root.join("unit-state");
        let restarts = root.join("unit-restarts");
        let commands = root.join("systemctl-commands.log");
        let program = root.join("fake-systemctl.sh");
        let body = FAKE_SYSTEMCTL
            .replace("__STATE__", &state.display().to_string())
            .replace("__RESTARTS__", &restarts.display().to_string())
            .replace("__LOG__", &commands.display().to_string());
        fs::write(&program, body).expect("the stand-in is written");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
            .expect("the stand-in is runnable");
        FakeSystemd {
            program,
            state,
            restarts,
            commands,
        }
    }

    /// A `systemctl` that reports fine but refuses to start a unit, so the path where a
    /// run directory was opened and the start then failed can be driven (T-8).
    fn fake_systemctl_start_fails(tag: &str) -> FakeSystemd {
        let root = scratch_root(tag);
        let state = root.join("unit-state");
        let restarts = root.join("unit-restarts");
        let commands = root.join("systemctl-commands.log");
        let program = root.join("fake-systemctl.sh");
        let body = FAKE_SYSTEMCTL
            .replace(
                "start) printf '%s' active > \"$STATE\"; exit 0 ;;",
                "start) printf '%s\\n' 'Job for the unit failed' >&2; exit 1 ;;",
            )
            .replace("__STATE__", &state.display().to_string())
            .replace("__RESTARTS__", &restarts.display().to_string())
            .replace("__LOG__", &commands.display().to_string());
        fs::write(&program, body).expect("the stand-in is written");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
            .expect("the stand-in is runnable");
        FakeSystemd {
            program,
            state,
            restarts,
            commands,
        }
    }

    /// A `systemctl` that is present but cannot reach the user manager, which is what
    /// a machine with no user session looks like. Every verb fails with systemd's own
    /// sentence, so the paths that must refuse — or must not need the manager at all —
    /// can be driven on any host (T-1).
    fn fake_dead_systemctl(tag: &str) -> FakeSystemd {
        let root = scratch_root(tag);
        let program = root.join("dead-systemctl.sh");
        let body = "#!/bin/sh\nprintf '%s\\n' 'Failed to connect to bus: No medium found' >&2\nexit 1\n";
        fs::write(&program, body).expect("the stand-in is written");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
            .expect("the stand-in is runnable");
        FakeSystemd {
            program,
            state: root.join("unused-state"),
            restarts: root.join("unused-restarts"),
            commands: root.join("unused-commands"),
        }
    }

    impl FakeSystemd {
        fn set(&self, state: &str) {
            fs::write(&self.state, state).expect("the state is written");
        }

        fn set_restarts(&self, count: u32) {
            fs::write(&self.restarts, count.to_string()).expect("the restart count is written");
        }

        fn commands(&self) -> Vec<String> {
            fs::read_to_string(&self.commands)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    /// A supervisor whose `systemctl` is the stand-in, with an agent's directories
    /// prepared and — when asked — the switch to systemd mode already made.
    fn systemd_supervisor(
        tag: &str,
        fake: &FakeSystemd,
        mode: Mode,
    ) -> (crate::paths::Layout, AgentPaths, Supervisor) {
        let root = scratch_root(tag);
        let layout = crate::paths::Layout::rooted_at(&root);
        let agent = layout.ensure_agent("scribe").expect("the agent is prepared");
        let credentials = layout.root().join("credentials.toml");
        crate::mode::write_private(&credentials, b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n")
            .expect("the credentials file is written");
        let mut supervisor = Supervisor::new(
            layout.state_root(),
            PathBuf::from("/usr/local/bin/gewu-multiverse"),
            10,
            Duration::from_secs(30),
        );
        supervisor.set_credentials(credentials);
        supervisor.set_config_root(layout.root());
        supervisor.set_unit_dir(layout.root().join("units"));
        supervisor.set_systemctl(Systemctl::with_binary(&fake.program));
        supervisor.load_modes(std::slice::from_ref(&agent));
        if mode == Mode::Systemd {
            let change = supervisor
                .switch_mode(&agent, Mode::Systemd)
                .expect("the switch is made");
            assert_eq!(change, ModeChange::Switched(Mode::Systemd));
        }
        (layout, agent, supervisor)
    }

    #[test]
    fn a_systemd_start_writes_the_unit_and_asks_the_users_systemd() {
        let fake = fake_systemctl("sup-systemd-start-fake");
        let (layout, agent, mut supervisor) =
            systemd_supervisor("sup-systemd-start", &fake, Mode::Systemd);
        assert_eq!(supervisor.mode("scribe"), Mode::Systemd);

        let disposition = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        assert!(matches!(disposition, StartDisposition::Started { .. }));

        let unit_path = layout.root().join("units").join("gewu-agent-scribe.service");
        assert!(unit_path.is_file(), "the unit is written");
        let unit = fs::read_to_string(&unit_path).expect("the unit reads");
        assert!(
            unit.contains("ExecStart=/usr/local/bin/gewu-multiverse --run-agent"),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure"), "{unit}");

        let run_id = supervisor
            .state("scribe")
            .run_id()
            .expect("a run id")
            .to_owned();
        let run_dir = supervisor.run_dir("scribe", &run_id);
        assert!(unit.contains(&run_dir.display().to_string()), "{unit}");
        assert!(run_dir.join(PLAN_FILE).is_file());
        let plan = RunPlan::read(&run_dir).expect("the plan reads");
        assert_eq!(plan.agent, "scribe");
        assert_eq!(plan.workspace, agent.workspace());

        let commands = fake.commands();
        eprintln!("systemctl calls: {commands:?}");
        assert!(
            commands.iter().any(|line| line == "--user daemon-reload"),
            "{commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|line| line == "--user start gewu-agent-scribe.service"),
            "{commands:?}"
        );

        match supervisor.state("scribe") {
            State::Running { pid, .. } => assert_eq!(pid, 4242),
            other => panic!("expected running, got {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 1);
        // The run the unit points at is on record, so a later session can find it.
        assert_eq!(
            crate::systemd::read_record(&agent).run_dir.as_deref(),
            Some(run_dir.as_path())
        );
    }

    #[test]
    fn stopping_a_systemd_agent_stops_the_unit_and_records_a_stopped_run() {
        let fake = fake_systemctl("sup-systemd-stop-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-systemd-stop", &fake, Mode::Systemd);
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let run_id = supervisor
            .state("scribe")
            .run_id()
            .expect("a run id")
            .to_owned();
        let run_dir = supervisor.run_dir("scribe", &run_id);

        assert_eq!(supervisor.stop("scribe"), StopDisposition::Signalled);
        assert!(
            fake.commands()
                .iter()
                .any(|line| line == "--user stop gewu-agent-scribe.service"),
            "{:?}",
            fake.commands()
        );
        match supervisor.state("scribe") {
            State::Finished {
                outcome: RunOutcome::Stopped,
                ..
            } => {}
            other => panic!("expected a stopped run, got {other:?}"),
        }
        match transcript::terminal_record(&run_dir) {
            Some(Record::RunFinished { outcome, .. }) => assert_eq!(outcome, "stopped"),
            other => panic!("expected a terminal record, got {other:?}"),
        }
    }

    #[test]
    fn an_inactive_unit_with_a_finished_run_is_shown_as_finished_not_running() {
        let fake = fake_systemctl("sup-systemd-inactive-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-systemd-inactive", &fake, Mode::Systemd);
        let run_dir = agent.run_dir("run-1-0001");
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let mut written = Transcript::open(&run_dir).expect("a transcript is opened");
        written
            .append(&Record::run_started("run-1-0001", "scribe", 1, "/w"))
            .expect("a record is appended");
        written
            .append(&Record::run_finished(
                "run-1-0001",
                RunOutcome::Succeeded,
                Some(0),
                10,
                None,
            ))
            .expect("a record is appended");
        crate::systemd::write_record(
            &agent,
            &Supervision {
                mode: Mode::Systemd,
                run_dir: Some(run_dir),
                restarts_recorded: 0,
            },
        )
        .expect("the record is written");
        fake.set("inactive");
        supervisor.load_modes(std::slice::from_ref(&agent));

        assert!(!supervisor.state("scribe").is_live());
        match supervisor.state("scribe") {
            State::Finished {
                outcome: RunOutcome::Succeeded,
                duration_ms: 10,
                ..
            } => {}
            other => panic!("expected a finished run, got {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 0);
    }

    #[test]
    fn a_unit_that_is_not_there_is_shown_as_idle() {
        let fake = fake_systemctl("sup-systemd-absent-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-systemd-absent", &fake, Mode::Systemd);
        fake.set("notfound");
        supervisor.load_modes(std::slice::from_ref(&agent));
        assert_eq!(supervisor.state("scribe"), State::Idle);
        assert_eq!(supervisor.state("scribe").word(), "idle");
    }

    #[test]
    fn a_mode_switch_is_refused_while_a_child_process_runs() {
        let binary = fake_agent("sup-switch-live", "sleep 30");
        let (layout, agent) = layout("sup-switch-live");
        let mut supervisor = supervisor(&layout, binary, 10, Duration::from_secs(30));
        supervisor.set_config_root(layout.root());
        supervisor.load_modes(std::slice::from_ref(&agent));
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        wait_for("running", || {
            supervisor.poll(Instant::now());
            matches!(supervisor.state("scribe"), State::Running { .. })
        });
        let error = supervisor
            .switch_mode(&agent, Mode::Systemd)
            .expect_err("the switch is refused");
        eprintln!("refusal while a UI child runs: {error}");
        assert!(error.contains("stop it first"), "{error}");
        assert!(error.contains("child of this UI"), "{error}");
        assert_eq!(supervisor.mode("scribe"), Mode::Tui);
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn a_mode_switch_is_refused_while_the_unit_is_active() {
        let fake = fake_systemctl("sup-switch-unit-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-switch-unit", &fake, Mode::Systemd);
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let error = supervisor
            .switch_mode(&agent, Mode::Tui)
            .expect_err("the switch is refused");
        eprintln!("refusal while the unit is active: {error}");
        assert!(error.contains("stop it first"), "{error}");
        assert!(error.contains("user unit"), "{error}");
        assert_eq!(supervisor.mode("scribe"), Mode::Systemd);
    }

    #[test]
    fn leaving_systemd_mode_removes_the_unit_file_and_is_idempotent() {
        let fake = fake_systemctl("sup-switch-leave-fake");
        let (layout, agent, mut supervisor) =
            systemd_supervisor("sup-switch-leave", &fake, Mode::Systemd);
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let unit_path = layout.root().join("units").join("gewu-agent-scribe.service");
        assert!(unit_path.is_file());
        fake.set("inactive");
        let change = supervisor
            .switch_mode(&agent, Mode::Tui)
            .expect("the switch is made");
        assert_eq!(change, ModeChange::Switched(Mode::Tui));
        assert!(!unit_path.exists(), "the unit file is removed");
        assert_eq!(supervisor.mode("scribe"), Mode::Tui);
        assert_eq!(
            supervisor
                .switch_mode(&agent, Mode::Tui)
                .expect("idempotent"),
            ModeChange::Already(Mode::Tui)
        );
        assert_eq!(crate::systemd::read_record(&agent).mode, Mode::Tui);
    }

    #[test]
    fn switching_into_systemd_is_refused_before_anything_is_written_when_the_manager_is_unreachable() {
        // T-1: `m` must not be a one-way door when the user manager cannot be reached
        // (a bare container, `su`, a plain `ssh`, some tmux/service contexts). The
        // switch is refused *before* the record is written or a unit is made, so what
        // is on screen (the `tui` row) stays true and nothing needs undoing.
        let fake = fake_dead_systemctl("sup-switch-refuse");
        let (layout, agent, mut supervisor) =
            systemd_supervisor("sup-switch-refuse", &fake, Mode::Tui);
        let error = supervisor
            .switch_mode(&agent, Mode::Systemd)
            .expect_err("the switch is refused");
        eprintln!("refusal with no user manager: {error}");
        assert!(error.contains("cannot be reached"), "{error}");
        assert!(error.contains("Nothing was changed"), "{error}");
        assert!(error.contains("tui"), "{error}");
        assert_eq!(supervisor.mode("scribe"), Mode::Tui);
        assert_eq!(crate::systemd::read_record(&agent).mode, Mode::Tui);
        assert!(
            !layout
                .root()
                .join("units")
                .join("gewu-agent-scribe.service")
                .exists(),
            "a refused switch writes no unit"
        );
    }

    #[test]
    fn a_systemd_start_that_fails_closes_the_run_it_opened_as_failed() {
        // T-8: a start that never ran must not be left for ever as a run with no
        // terminal record; its own record is closed with the reason.
        let fake = fake_systemctl_start_fails("sup-start-fail");
        let (layout, agent, mut supervisor) =
            systemd_supervisor("sup-start-fail", &fake, Mode::Systemd);
        let error = supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect_err("the start is refused");
        eprintln!("failed systemd start: {error}");

        let runs = layout.state_root().join("scribe");
        let mut checked = false;
        for entry in fs::read_dir(&runs).expect("the run directory is there") {
            let dir = entry.expect("an entry").path();
            if let Some(Record::RunFinished { outcome, reason, .. }) = transcript::terminal_record(&dir)
            {
                checked = true;
                assert_eq!(outcome, "failed", "{outcome}");
                assert!(
                    reason.unwrap_or_default().contains("the start failed"),
                    "the record names the start"
                );
            }
        }
        assert!(checked, "the opened run has a terminal record");
    }

    #[test]
    fn leaving_systemd_mode_needs_no_manager_when_no_unit_is_on_disk() {
        // T-1: a `systemd` record left by an earlier session (when the manager was
        // reachable) with no unit file on disk is not a one-way door: leaving it asks
        // systemd nothing, because there is nothing to stop.
        let fake = fake_dead_systemctl("sup-switch-back");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-switch-back", &fake, Mode::Tui);
        crate::systemd::write_record(
            &agent,
            &Supervision {
                mode: Mode::Systemd,
                run_dir: None,
                restarts_recorded: 0,
            },
        )
        .expect("the record is written");
        supervisor.load_modes(std::slice::from_ref(&agent));
        assert_eq!(supervisor.mode("scribe"), Mode::Systemd);
        let change = supervisor
            .switch_mode(&agent, Mode::Tui)
            .expect("the switch back is made without the manager");
        assert_eq!(change, ModeChange::Switched(Mode::Tui));
        assert_eq!(supervisor.mode("scribe"), Mode::Tui);
        assert_eq!(crate::systemd::read_record(&agent).mode, Mode::Tui);
    }

    #[test]
    fn a_mode_read_again_forgets_a_unit_of_an_agent_that_left_systemd_mode() {
        let fake = fake_systemctl("sup-forget-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-forget", &fake, Mode::Systemd);
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        assert_eq!(supervisor.running_count(), 1);
        // The record is changed to the default mode outside the tool; reading the
        // agents again must not keep counting a unit that is no longer the agent's.
        crate::systemd::write_record(&agent, &Supervision::default()).expect("written");
        supervisor.load_modes(std::slice::from_ref(&agent));
        assert_eq!(supervisor.mode("scribe"), Mode::Tui);
        assert_eq!(supervisor.running_count(), 0);
        assert_eq!(supervisor.state("scribe"), State::Idle);
    }

    #[test]
    fn a_restart_systemd_reports_is_written_into_the_run_log_once() {
        let fake = fake_systemctl("sup-systemd-restart-fake");
        let (_layout, agent, mut supervisor) =
            systemd_supervisor("sup-systemd-restart", &fake, Mode::Systemd);
        supervisor
            .start(&agent, &RunModel::default(), Vec::new())
            .expect("started");
        let run_id = supervisor
            .state("scribe")
            .run_id()
            .expect("a run id")
            .to_owned();
        let run_dir = supervisor.run_dir("scribe", &run_id);

        fake.set_restarts(1);
        supervisor.systemd.poll_now();
        let log = fs::read_to_string(run_dir.join(LOG_FILE)).expect("the log reads");
        assert!(
            log.contains("systemd restarted this run (restart 1 of at most 3)"),
            "{log}"
        );
        assert_eq!(supervisor.restart_count("scribe"), 1);

        // Reading again does not write it again.
        supervisor.systemd.poll_now();
        let log = fs::read_to_string(run_dir.join(LOG_FILE)).expect("the log reads");
        assert_eq!(
            log.matches("systemd restarted this run").count(),
            1,
            "{log}"
        );
    }

    #[test]
    fn a_failure_reason_names_its_class_for_the_state_cell() {
        // F-G: `failed (transport)` is more use than `failed`.
        assert_eq!(
            failure_class("the model call failed (transport): connection refused"),
            Some("transport")
        );
        assert_eq!(
            failure_class("the model key cannot be read: no key"),
            Some("missing_key")
        );
        assert_eq!(failure_class("run run-1 exited with code 1"), None);
    }

    #[test]
    fn a_systemd_record_with_no_run_still_reports_a_manager_that_is_gone() {
        // M-6: the product writes `mode = "systemd", run_dir = None` when switching,
        // and nothing read that record's manager, so `systemd` + `idle` was shown with
        // no warning that the first start would fail.
        let fake = fake_dead_systemctl("sup-untracked-dead");
        let root = scratch_root("sup-untracked");
        let layout = crate::paths::Layout::rooted_at(&root);
        let agent = layout.ensure_agent("scribe").expect("the agent is prepared");
        let mut supervisor = Supervisor::new(
            layout.state_root(),
            PathBuf::from("/usr/local/bin/gewu-multiverse"),
            10,
            Duration::from_secs(30),
        );
        supervisor.set_unit_dir(root.join("units"));
        supervisor.set_systemctl(Systemctl::with_binary(&fake.program));
        // The record the product itself writes on a switch: systemd, no run.
        crate::systemd::write_record(
            &agent,
            &crate::systemd::Supervision {
                mode: Mode::Systemd,
                run_dir: None,
                restarts_recorded: 0,
            },
        )
        .expect("the record is written");
        supervisor.load_modes(std::slice::from_ref(&agent));
        assert_eq!(supervisor.mode("scribe"), Mode::Systemd);
        supervisor.poll(Instant::now());
        assert!(
            supervisor.systemd_last_error().is_some(),
            "a systemd record with no run hid an unreachable manager"
        );
    }
}
