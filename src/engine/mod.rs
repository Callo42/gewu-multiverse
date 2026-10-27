//! The engine behind the board: one bundled Forgejo, the agents connected to it, and
//! the sessions that keep running until they are stopped.
//!
//! `docs/engine.md` is the design. The short version:
//!
//! - the bundled Forgejo is installed, prepared and run by
//!   [`crate::bundled_forgejo`], under the engine's own data lock;
//! - `CreateAgents`/`AddAgent` make `agent-1` … each with its own managed Forgejo
//!   account and membership of team `agents`;
//! - `Start` records the started choice and runs the existing agent run again and
//!   again, with a back-off when it ends, until `Stop`;
//! - `board()` reads memory only; `send()` returns at once; slow work runs on
//!   background threads;
//! - `shutdown(grace)` stops every session and the server and leaves no process.

pub mod clock;
pub mod last_seen;
pub mod session;
pub mod settings;

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use nix::fcntl::{Flock, FlockArg};

use crate::agent_run;
use crate::bundled_forgejo::{self, PreparedInstance, RunningForgejo};
use crate::credentials::{self, CredentialsFile, Secret};
use crate::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, ProgramView, Provisioning, RunProgramView, SessionEnd, SessionState, UpgradeAgent,
    UpgradeView,
};
use crate::forgejo::{DEFAULT_TIMEOUT, ForgejoClient, ForgejoError, Repository};
use crate::harness::{self, Program, Protocol};
use crate::identity;
use crate::mode::{PRIVATE_DIR_MODE, SECRET_FILE_MODE, set_mode};
use crate::paths::Layout;
use crate::run::process_alive;
use crate::run::{self, LOG_FILE};
use std::os::unix::process::ExitStatusExt;

use crate::transcript::TRANSCRIPT_FILE;

use self::clock::{RealClock, SharedClock};
#[cfg(test)]
use self::session::FakeSpawner;
use self::session::{
    NativeRequest, PreparedSession, RealSpawner, SessionRepository, SessionRequest, SessionSpawner,
};
use self::settings::{AgentSettings, MAX_AGENTS, Settings};

/// How long one session may run before the engine stops it.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How long a stopped process may take to end politely.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);

/// How long the bundled Forgejo may take to answer after it starts.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// The delay after a session that succeeded or ran at least this long.
pub const RESTART_DELAY: Duration = Duration::from_secs(5);

/// The documented wait after a successful run that found no work.
///
/// A no-work run is a success, not a quick failure: it resets the failure streak
/// and waits the fixed, documented 20 s rather than entering the doubling back-off.
pub const NO_WORK_DELAY: Duration = Duration::from_secs(20);

/// A session shorter than this is a quick failure.
pub const LONG_SESSION: Duration = Duration::from_secs(60);

/// The first quick-failure delay.
pub const QUICK_FAILURE_DELAY: Duration = Duration::from_secs(10);

/// The largest quick-failure delay.
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);

/// The restart back-off for the Forgejo child.
pub const FORGEJO_RESTART_BASE: Duration = Duration::from_secs(3);

/// The largest restart back-off for the Forgejo child.
pub const FORGEJO_RESTART_MAX: Duration = Duration::from_secs(60);

/// How long to wait before retrying an account that could not be provisioned.
pub const PROVISION_RETRY: Duration = Duration::from_secs(5);

/// How often the organisation's repositories are listed again. Two seconds keeps a
/// deletion's stale window short on a local server, and the count is replaced in the
/// same tick as its event (W31-8).
pub const REPOSITORY_REFRESH: Duration = Duration::from_secs(2);

/// The terms the first-run screen shows, one short item each. They are the product's
/// fixed words, in the first-time reader's language.
pub const TERMS_VERSION: u32 = 2;

/// The one line from this release's notes that the upgrade screen shows.
///
/// It stays inside what the product can observe: marks name the evidence the run
/// reported, and an ordinary earlier run is stopped with its cleanup checked. It does
/// not promise perfect shell interpretation or containment of detached processes.
pub const WHATS_NEW: &str = "run marks name what was observed; an ordinary earlier run is stopped and its cleanup checked before the board reopens";

/// The terms the first-run screen shows, one short item each. They are the product's
/// fixed words, in the first-time reader's language.
pub const TERMS: &[&str] = &[
    "Agents use this computer with your permissions.",
    "When runs are confined, they cannot read the product's keys, your home or another agent's files, and they start with a cleared environment.",
    "Each has its own login on your Forgejo.",
    "Started agents keep going until you stop them.",
    "Their work is sent to your model provider.",
];

/// How the engine is opened.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// The configuration and state roots this installation uses.
    pub layout: Layout,
    /// Where the bundled Forgejo lives.
    pub data_root: PathBuf,
    /// The product executable, which runs each session as `--run-agent`.
    pub product_binary: PathBuf,
    /// The session timeout.
    pub session_timeout: Duration,
    /// The polite-stop grace.
    pub stop_grace: Duration,
    /// How long the Forgejo may take to answer.
    pub ready_timeout: Duration,
    /// Start an already-installed Forgejo when the engine opens.
    pub auto_start_forgejo: bool,
    /// How this computer can confine an agent's shell. It is detected once, when
    /// the engine opens, so the board and the warnings can say the truth before a
    /// run starts.
    pub confinement: crate::confinement::Method,
    /// The tests' simulated readiness, never set in production.
    #[cfg(test)]
    pub(crate) forgejo_ready: bool,
    /// The tests' starter, which can block after the child exists. Production uses
    /// the bundled launcher.
    #[cfg(test)]
    forgejo_starter: Option<Arc<dyn ForgejoStarter>>,
}

impl EngineOptions {
    /// Options for one installation, with the product executable found now.
    pub fn new(layout: Layout, data_root: impl Into<PathBuf>) -> Result<Self, String> {
        let product_binary = std::env::current_exe()
            .map_err(|error| format!("the product executable cannot be found: {error}"))?;
        let data_root = data_root.into();
        // The mechanism is chosen against this installation's own roots and **every
        // real agent workspace**, not only the directory that contains them: a
        // workspace symlinked over a parent of the configuration root cannot be
        // confined, and `detect` must see that before the board claims anything
        // (B9-R-R).
        let workspaces = crate::confinement::agent_workspaces(&layout.agents_dir());
        let confinement =
            crate::confinement::detect(layout.root(), layout.state_root(), &data_root, &workspaces);
        Ok(Self {
            layout,
            data_root,
            product_binary,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
            stop_grace: DEFAULT_STOP_GRACE,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            auto_start_forgejo: true,
            confinement,
            #[cfg(test)]
            forgejo_ready: false,
            #[cfg(test)]
            forgejo_starter: Some(Arc::new(RealForgejoStarter)),
        })
    }
}

/// How the engine starts one Forgejo instance.
///
/// Production calls [`bundled_forgejo::spawn_prepared`] with the instance the start
/// job already prepared. The tests use it to hold a child after it exists and before
/// the readiness wait, so the shutdown interleavings can be driven exactly.
#[cfg(test)]
trait ForgejoStarter: Send + Sync + std::fmt::Debug {
    fn start(
        &self,
        binary: &Path,
        prepared: PreparedInstance,
        ready_timeout: Duration,
        on_spawn: &mut dyn FnMut(u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<RunningForgejo, String>;
}

#[cfg(test)]
#[derive(Debug)]
struct RealForgejoStarter;

#[cfg(test)]
impl ForgejoStarter for RealForgejoStarter {
    fn start(
        &self,
        binary: &Path,
        prepared: PreparedInstance,
        ready_timeout: Duration,
        on_spawn: &mut dyn FnMut(u32),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<RunningForgejo, String> {
        bundled_forgejo::spawn_prepared(prepared, binary, ready_timeout, on_spawn, cancelled)
    }
}

/// Spawn one Forgejo through the tests' starter when one is installed, else through
/// the bundled launcher, from an instance the job has already prepared.
fn start_forgejo(
    _options: &EngineOptions,
    binary: &Path,
    prepared: PreparedInstance,
    ready_timeout: Duration,
    on_spawn: &mut dyn FnMut(u32),
    cancelled: &dyn Fn() -> bool,
) -> Result<RunningForgejo, String> {
    #[cfg(test)]
    if let Some(starter) = _options.forgejo_starter.as_ref() {
        return starter.start(binary, prepared, ready_timeout, on_spawn, cancelled);
    }
    bundled_forgejo::spawn_prepared(prepared, binary, ready_timeout, on_spawn, cancelled)
}

/// The engine, as the board holds it.
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    /// The data-directory lock, held until this value drops.
    _lock: Flock<File>,
    clock: SharedClock,
    spawner: Arc<dyn SessionSpawner>,
    options: EngineOptions,
}

/// Open the engine for the current user's installation.
pub fn open() -> Result<Engine, String> {
    let layout = Layout::in_user_config_dir()
        .map_err(|error| format!("the installation's directories cannot be found: {error}"))?;
    let data_root = bundled_forgejo::data_root()?;
    open_with(EngineOptions::new(layout, data_root)?)
}

/// Open the engine under a chosen configuration root, with the default data root.
pub fn open_at(layout: Layout) -> Result<Engine, String> {
    let data_root = bundled_forgejo::data_root()?;
    open_with(EngineOptions::new(layout, data_root)?)
}

/// Open the engine with explicit options.
pub fn open_with(options: EngineOptions) -> Result<Engine, String> {
    let clock: SharedClock = Arc::new(RealClock);
    let spawner: Arc<dyn SessionSpawner> =
        Arc::new(RealSpawner::new(options.product_binary.clone()));
    build(options, clock, spawner, None)
}

/// Open with a test's clock, spawner and settings. The session loop is exercised
/// without sleeping and without a real Forgejo.
#[cfg(test)]
pub(crate) fn open_for_test(
    mut options: EngineOptions,
    clock: SharedClock,
    spawner: Arc<dyn SessionSpawner>,
    settings: Settings,
) -> Result<Engine, String> {
    // The test's product binary need not be the test executable.
    if options.product_binary.as_os_str().is_empty() {
        options.product_binary = PathBuf::from("/bin/true");
    }
    build(options, clock, spawner, Some(settings))
}

/// Open an engine whose settings are already loaded.
fn build(
    options: EngineOptions,
    clock: SharedClock,
    spawner: Arc<dyn SessionSpawner>,
    settings: Option<Settings>,
) -> Result<Engine, String> {
    options
        .layout
        .ensure_root()
        .map_err(|error| error.to_string())?;
    let settings_path = settings::path_in(options.layout.root());
    let settings = match settings {
        Some(settings) => settings,
        None => Settings::load(&settings_path)?,
    };
    for agent in &settings.agents {
        if !crate::naming::is_safe_agent_name(&agent.name) {
            return Err(format!(
                "the settings name `{}`, which cannot name an agent",
                agent.name
            ));
        }
    }
    let credentials = options.layout.credentials_file();
    CredentialsFile::prepare(&credentials)
        .map_err(|error| format!("the credentials file cannot be prepared: {error}"))?;
    let lock = acquire_lock(&options.data_root)?;
    // A killed program leaves its Forgejo and its sessions running. They are found by
    // their recorded pids, believed only when their command line is the one this
    // product started, and stopped before a fresh one can start.
    bundled_forgejo::recover_stale_server(&options.data_root, options.stop_grace)?;
    let (_, leftovers) = recover_stale_sessions(&options.layout, options.stop_grace)?;
    remove_stale_run_temps();
    // A kill between the installer's two directory renames is restored before the
    // first picture reads the program cards. A live install owns its directory and
    // is not touched.
    let install_recovery = harness::recover_pending(&options.data_root);
    let upgrade = upgrade_view(
        &options.layout,
        &settings,
        &settings_path,
        &options.data_root,
    );
    // The recovery result is part of the first picture: Inner::new must receive it
    // before it loads the agent cards, or a failed cleanup is silently dropped and
    // the engine starts a replacement run beside the old process (review-3).
    let mut inner = Inner::new(
        options.clone(),
        settings_path,
        settings,
        credentials,
        leftovers,
    )?;
    inner.upgrade = upgrade;
    for note in install_recovery {
        inner.note(note);
    }
    last_seen::write(options.layout.root(), env!("CARGO_PKG_VERSION"))?;
    // An installation from the old screens may still hold `config.toml`. The new
    // engine does not read it and does not convert it; it says once, on the first
    // screen, that the earlier setup was left as it was.
    if inner.options.layout.config_file().is_file() {
        inner.note("an earlier setup was found and left as it was");
    }
    let engine = Engine {
        inner: Arc::new(Mutex::new(inner)),
        _lock: lock,
        clock,
        spawner,
        options,
    };
    Ok(engine)
}

/// What opening this installation will stop before the board can appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryClaim {
    /// The agent whose live run will be stopped, when one was found.
    pub agent: Option<String>,
    /// The id of that run.
    pub run_id: Option<String>,
    /// The bundled Forgejo is also live and will be stopped.
    pub forgejo: bool,
}

impl RecoveryClaim {
    /// The one sentence the opening screen shows. It names the agent and the run,
    /// not "the earlier run", and still names Forgejo when that is being stopped too.
    pub fn message(&self) -> String {
        let run = match (&self.agent, &self.run_id) {
            (Some(agent), Some(run_id)) => Some(format!("{agent}'s run {run_id}")),
            (Some(agent), None) => Some(format!("{agent}'s run")),
            _ => None,
        };
        match (run, self.forgejo) {
            (Some(run), true) => format!("stopping {run} and Forgejo…"),
            (Some(run), false) => format!("stopping {run}…"),
            (None, true) => "stopping Forgejo…".to_owned(),
            (None, false) => String::new(),
        }
    }
}

/// Whether opening this installation will first stop an earlier run or Forgejo.
///
/// This is read before the engine is built so the board can say what the wait is
/// for instead of showing a blank alternate screen. Only live records count: a
/// stale pid file with no live process is cleanup, not a process being stopped.
pub fn stale_recovery_pending(layout: &Layout, data_root: &Path) -> bool {
    stale_recovery_claim(layout, data_root).is_some()
}

/// The same check, with the agent and run the opening screen names.
pub fn stale_recovery_claim(layout: &Layout, data_root: &Path) -> Option<RecoveryClaim> {
    let mut live_agent = None;
    let mut live_run = None;
    if let Ok(agents) = fs::read_dir(layout.state_root()) {
        'agents: for agent in agents.flatten() {
            let Ok(runs) = fs::read_dir(agent.path()) else {
                continue;
            };
            for run_dir in runs.flatten() {
                if !run_dir.path().is_dir() {
                    continue;
                }
                if let Some((pid, _)) = run::recorded_group(&run_dir.path())
                    && run::process_alive(pid)
                {
                    live_agent = agent.file_name().to_str().map(str::to_owned);
                    live_run = run_dir.file_name().to_str().map(str::to_owned);
                    break 'agents;
                }
            }
        }
    }
    let forgejo = {
        let record = bundled_forgejo::InstancePaths::under(data_root)
            .root
            .join(bundled_forgejo::PID_FILE);
        fs::read_to_string(&record)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .is_some_and(run::process_alive)
    };
    (live_run.is_some() || forgejo).then_some(RecoveryClaim {
        agent: live_agent,
        run_id: live_run,
        forgejo,
    })
}

/// What the one upgrade screen should say, when this is the first open by a newer
/// build. A fresh installation, a same-version open and a downgrade have no screen.
fn upgrade_view(
    layout: &Layout,
    settings: &Settings,
    settings_path: &Path,
    data_root: &Path,
) -> Option<UpgradeView> {
    // Builds before 0.1.31 wrote no marker at all, so a real upgrade from 0.1.30 has
    // none. An existing installation (its settings file, or the older text screens'
    // config.toml) still deserves the one screen: the installer writes the exact old
    // version before it replaces the binary, and this fallback covers a binary
    // replaced by other means. A fresh root writes neither file, so it stays silent.
    let from = match last_seen::read(layout.root()) {
        Some(version) => version,
        None if settings_path.is_file() || layout.config_file().is_file() => {
            "an earlier version".to_owned()
        }
        None => return None,
    };
    if from != "an earlier version" && !last_seen::is_older(&from, env!("CARGO_PKG_VERSION")) {
        return None;
    }
    let mut instructions = 0;
    let mut runs = 0;
    if let Ok(agents) = fs::read_dir(layout.agents_dir()) {
        for agent in agents.flatten() {
            let path = agent.path().join("instruction.md");
            if fs::read_to_string(&path)
                .map(|text| !agent_run::instruction_is_blank(text.as_bytes()))
                .unwrap_or(false)
            {
                instructions += 1;
            }
        }
    }
    if let Ok(agents) = fs::read_dir(layout.state_root()) {
        for agent in agents.flatten() {
            if let Ok(runs_of_agent) = fs::read_dir(agent.path()) {
                runs += runs_of_agent
                    .flatten()
                    .filter(|run| run::looks_like_run_id(&run.file_name().to_string_lossy()))
                    .count();
            }
        }
    }
    let kept_agents = settings
        .agents
        .iter()
        .map(|agent| {
            let program = agent.harness();
            let provider = agent.provider();
            UpgradeAgent {
                name: agent.name.clone(),
                program: program.label().to_owned(),
                model: provider
                    .map(|provider| provider.model.clone())
                    .filter(|model| !model.trim().is_empty())
                    .unwrap_or_else(|| settings.endpoint.model.clone()),
                protocol: provider
                    .map(|provider| provider.protocol().label().to_owned())
                    .unwrap_or_else(|| settings.endpoint.protocol().label().to_owned()),
            }
        })
        .collect();
    let programs = crate::harness::installed_all(data_root)
        .into_iter()
        .filter(|(_, installed)| installed.is_some())
        .count();
    Some(UpgradeView {
        from_version: from,
        to_version: env!("CARGO_PKG_VERSION").to_owned(),
        settings: settings_path.is_file() || layout.config_file().is_file(),
        agents: settings.agents.len(),
        instructions,
        runs,
        ignored_config: layout.config_file().is_file(),
        kept_agents,
        programs,
        // The screen names this build's own release notes, not the list of every
        // release: the person wants the exact version they just opened.
        release_url: format!(
            "https://github.com/{}/releases/tag/v{}",
            bundled_forgejo::REPOSITORY,
            env!("CARGO_PKG_VERSION")
        ),
        whats_new: WHATS_NEW.to_owned(),
        warnings_changed: settings.terms_version < TERMS_VERSION,
    })
}

/// Take the one-instance-per-data-directory lock. A second program fails here,
/// before it can touch the first program's instance.
fn acquire_lock(data_root: &Path) -> Result<Flock<File>, String> {
    let dir = bundled_forgejo::forgejo_dir(data_root);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
    set_mode(&dir, 0o755)
        .map_err(|error| format!("{} cannot be set to mode 0755: {error}", dir.display()))?;
    let path = bundled_forgejo::lock_path(data_root);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(SECRET_FILE_MODE)
        .open(&path)
        .map_err(|error| format!("{} cannot be opened: {error}", path.display()))?;
    // Closing the terminal leaves the old copy stopping its Forgejo and runs for a
    // moment; the lock is released when that finishes. Wait for it rather than
    // refusing a reopen that would have worked a second later (P8).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut file = file;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((returned, error))
                if error == nix::errno::Errno::EWOULDBLOCK
                    || error == nix::errno::Errno::EAGAIN =>
            {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "the previous copy is still stopping — try again in a moment (it is using {})",
                        data_root.display()
                    ));
                }
                file = returned;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err((_, error)) => {
                return Err(format!("{} cannot be locked: {error}", path.display()));
            }
        }
    }
}

/// One running session.
struct RunningState {
    run_dir: PathBuf,
    started: Instant,
    run: run::StartedRun,
    stopping: bool,
    stop_deadline: Option<Instant>,
    stop_requested: bool,
    timed_out: bool,
    /// The run's first ending, recorded once when it happens. A later Stop,
    /// shutdown, settle, reap or reopen never changes it.
    first_ending: Option<run::FirstEnding>,
    /// The last failure to persist the first-ending fact, if any. It is retried on
    /// the next deadline tick; the run is not signalled until the fact is durable.
    first_ending_error: Option<String>,
}

/// The facts `poll_agent` hands from the running state to the settle boundary.
type EndedRun = (
    SessionEnd,
    Instant,
    PathBuf,
    u32,
    bool,
    Option<run::NativeProgress>,
    Option<run::FirstEnding>,
);

/// One agent's engine state, beside its saved settings.
/// A recovery scope whose cleanup failed: one run directory, or an agent
/// directory that could not be enumerated and must be rescanned (review-3b).
#[derive(Debug, Clone)]
enum RecoveryTarget {
    Run(PathBuf),
    Agent(PathBuf),
}

struct AgentRuntime {
    name: String,
    desired: Desired,
    provisioning: Provisioning,
    login: Option<String>,
    email: Option<String>,
    session: SessionState,
    sessions_since_start: u64,
    /// Every run record found on open plus every run started since; never reset by a
    /// Start. This is what the card's `run N` shows.
    runs_total: u64,
    session_number: u64,
    recent: VecDeque<SessionEnd>,
    instruction_chars: usize,
    instruction_first_line: Option<String>,
    instruction: Option<String>,
    instruction_lines: usize,
    instruction_mtime: Option<SystemTime>,
    activity: Option<String>,
    activity_at: Option<SystemTime>,
    last_error: Option<String>,
    last_result: Option<String>,
    last_failure: Option<String>,
    /// A left-over process group this engine could not reap, one plain sentence.
    leftover: Option<String>,
    /// The recovery scopes whose cleanup failed, so Start can retry the reap or
    /// rescan before any replacement run is allowed.
    leftover_targets: Vec<RecoveryTarget>,
    current_tool: Option<String>,
    current_tool_since: Option<SystemTime>,
    /// A temporary provider failure the live native program is retrying.
    retrying: Option<String>,
    last_work: Vec<String>,
    no_assigned_work: bool,
    last_work_result: Option<String>,
    last_result_is_work: bool,
    activity_refresh_at: Option<Instant>,
    newest_run: Option<PathBuf>,
    running: Option<RunningState>,
    starting: bool,
    starting_pid: Option<u32>,
    starting_since: Option<Instant>,
    next_at: Option<Instant>,
    last_end: Option<SessionEnd>,
    quick_failures: u32,
    provision_inflight: bool,
    provision_retry_at: Option<Instant>,
    removal_pending: bool,
    removal_retry_at: Option<Instant>,
    /// Why the chosen program cannot start, with the exact action, kept after the
    /// status line fades. It is the Agent screen's persistent prerequisite line.
    program_hint: Option<String>,
}

/// The bundled Forgejo's engine state.
struct ForgejoRuntime {
    state: ForgejoState,
    url: String,
    admin_login: String,
    admin_login_kept: bool,
    admin_token: Option<Secret>,
    child: Option<RunningForgejo>,
    /// The pid of a Forgejo child whose start has not returned yet.
    start_pid: Option<u32>,
    start_inflight: bool,
    /// Access-token helpers started by the start or provisioning job. They are
    /// recorded before they run and are visible to `shutdown`, like `start_pid`.
    helper_pids: Vec<u32>,
    /// How many token helpers are between "about to spawn" and "reaped". Visible to
    /// `shutdown` under the same lock, so a helper cannot be spawned after the final
    /// force phase has passed.
    helper_inflight: usize,
    start_requested: bool,
    retry_start: bool,
    retry_at: Option<Instant>,
    failures: u32,
    provisioned: bool,
    provision_inflight: bool,
    provision_retry_at: Option<Instant>,
    repositories: Vec<Repository>,
    /// Repository changes seen between administration-API polls, oldest first.
    events: VecDeque<String>,
    repositories_refreshed_at: Option<Instant>,
    repositories_inflight: bool,
}

/// The engine's mutable state.
struct Inner {
    options: EngineOptions,
    settings: Settings,
    settings_path: PathBuf,
    credentials: PathBuf,
    agents: Vec<AgentRuntime>,
    forgejo: ForgejoRuntime,
    endpoint_test: EndpointTest,
    /// The newest test of one agent's own saved connection, keyed by agent name.
    agent_tests: HashMap<String, EndpointTest>,
    notice: Option<String>,
    admin_password_once: Option<String>,
    password_reset_inflight: bool,
    upgrade: Option<UpgradeView>,
    /// Left-over process groups the last open could not stop: agent, run
    /// directory and the reason, so the card can keep both the sentence and a
    /// retry route.
    leftovers: Vec<(String, RecoveryTarget, String)>,
    sequence: u64,
    stopping: bool,
    generation: u64,
    /// Install progress/errors for the Program screen.
    program_states: HashMap<String, ProgramState>,
}

#[derive(Default)]
struct ProgramState {
    installing: Option<Option<u8>>,
    phase: Option<harness::InstallPhase>,
    /// When the current install began, so a silent npm step still shows an honest
    /// elapsed time instead of a frozen phrase.
    started: Option<Instant>,
    error: Option<String>,
    cancel: Option<harness::CancelToken>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Inner {
    fn new(
        options: EngineOptions,
        settings_path: PathBuf,
        settings: Settings,
        credentials: PathBuf,
        leftovers: Vec<(String, RecoveryTarget, String)>,
    ) -> Result<Self, String> {
        let binary = bundled_forgejo::binary_path(&options.data_root);
        let paths = bundled_forgejo::InstancePaths::under(&options.data_root);
        let port = fs::read_to_string(&paths.app_ini)
            .ok()
            .and_then(|text| bundled_forgejo::port_from_config(&text));
        let recorded_login = fs::read_to_string(&paths.admin_login_file)
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| bundled_forgejo::valid_username(text));
        let admin_login = if !settings.forgejo.admin_login.trim().is_empty() {
            settings.forgejo.admin_login.clone()
        } else {
            recorded_login
                .clone()
                .unwrap_or_else(bundled_forgejo::admin_login_from_env)
        };
        // An earlier version used the computer account name. It stays the working
        // login for that installation; the page says so, and a new installation
        // uses the fixed default instead.
        let admin_login_kept =
            !admin_login.is_empty() && admin_login != bundled_forgejo::admin_login_from_env();
        let state = if binary.is_file() {
            ForgejoState::Stopped
        } else {
            ForgejoState::Missing
        };
        #[cfg(test)]
        let state = if options.forgejo_ready {
            ForgejoState::Running
        } else {
            state
        };
        let repositories = Vec::new();
        let mut events = VecDeque::new();
        if let Some(event) = settings.forgejo.last_event.clone() {
            events.push_back(event);
        }
        let mut forgejo = ForgejoRuntime {
            state,
            url: port
                .map(bundled_forgejo::InstancePaths::url)
                .unwrap_or_default(),
            admin_login,
            admin_login_kept,
            admin_token: None,
            child: None,
            start_pid: None,
            start_inflight: false,
            helper_pids: Vec::new(),
            helper_inflight: 0,
            start_requested: false,
            retry_start: false,
            retry_at: None,
            failures: 0,
            provisioned: false,
            provision_inflight: false,
            provision_retry_at: None,
            repositories,
            events,
            repositories_refreshed_at: None,
            repositories_inflight: false,
        };
        #[cfg(test)]
        if options.forgejo_ready {
            forgejo.provisioned = true;
        }
        if options.auto_start_forgejo {
            // The first start installs the binary when it is not there yet; the build
            // picture reports the download's progress.
            forgejo.start_requested = true;
            forgejo.retry_at = None;
        }
        let mut inner = Self {
            options,
            settings,
            settings_path,
            credentials,
            agents: Vec::new(),
            forgejo,
            endpoint_test: EndpointTest::NotRun,
            agent_tests: HashMap::new(),
            notice: None,
            admin_password_once: None,
            password_reset_inflight: false,
            upgrade: None,
            leftovers,
            sequence: 0,
            stopping: false,
            generation: 0,
            program_states: HashMap::new(),
        };
        // A number this installation has ever used is never reused, even when the
        // agent is gone from the settings. The names on disk are the floor for an
        // upgrade from a version that kept no high-water mark (U5).
        inner.observe_used_agent_numbers();
        let names = inner.settings.names();
        for name in names {
            inner.agents.push(AgentRuntime::load(&inner, &name));
        }
        // A group the last open could not reap is reported on its agent's card,
        // not hidden in the recovery log (F-02/F-03). The run directory is kept
        // so Start can retry the reap, and the blocked state stops the scheduler
        // from starting a replacement beside a real old process.
        for (name, target, reason) in inner.leftovers.clone() {
            if let Some(index) = inner.agent_index(&name) {
                inner.agents[index].session = SessionState::Blocked {
                    reason: reason.clone(),
                };
                inner.agents[index].leftover = Some(reason);
                inner.agents[index].leftover_targets.push(target);
                inner.agents[index].next_at = None;
            }
        }
        #[cfg(test)]
        if inner.options.forgejo_ready {
            for agent in &mut inner.agents {
                agent.provisioning = Provisioning::Ready;
                agent.login = Some(format!("gewu-{}", agent.name));
                agent.email = Some(crate::identity::email_for(&format!("gewu-{}", agent.name)));
            }
        }
        Ok(inner)
    }

    fn credential_key_present(&self) -> bool {
        credentials::model_key(&self.credentials, "").is_ok()
    }

    fn save_settings(&self) -> Result<(), String> {
        self.settings.save(&self.settings_path)
    }

    /// Apply one settings command to a copy, persist the copy, and only then make it
    /// live. A failed validation or save leaves the in-memory settings untouched.
    fn commit_settings<F>(&mut self, change: F) -> Result<(), String>
    where
        F: FnOnce(&mut settings::Settings) -> Result<(), String>,
    {
        let mut next = self.settings.clone();
        change(&mut next)?;
        next.save(&self.settings_path)?;
        self.settings = next;
        Ok(())
    }

    fn agent_index(&self, name: &str) -> Option<usize> {
        self.agents.iter().position(|agent| agent.name == name)
    }

    /// Raise the high-water mark to every agent name this installation has ever had
    /// on disk. The configuration and state directories survive a removal because
    /// the history is kept, so they still name a removed agent (U5).
    fn observe_used_agent_numbers(&mut self) {
        let mut names = self.settings.names();
        for root in [
            self.options.layout.agents_dir(),
            self.options.layout.state_root().to_path_buf(),
        ] {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_owned());
                    }
                }
            }
        }
        for name in names {
            self.settings.observe_agent_name(&name);
        }
    }

    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notice = Some(crate::forgejo_tools::sentence(text.into()));
    }

    /// Recompute the mechanism after the set of agent workspaces changed. A new
    /// workspace that cannot be confined takes the board's claim away now, instead of
    /// waiting for a run to refuse it (B9-R-R).
    fn refresh_confinement(&mut self) {
        let workspaces = crate::confinement::agent_workspaces(&self.options.layout.agents_dir());
        let method = crate::confinement::detect(
            self.options.layout.root(),
            self.options.layout.state_root(),
            &self.options.data_root,
            &workspaces,
        );
        if method != self.options.confinement {
            self.options.confinement = method;
            if !method.confined() {
                self.note("Runs are not confined on this computer.");
            }
        }
    }

    /// A run whose plan promised a boundary the run-time layout could not keep left a
    /// marker. Take that refusal onto the board and the warnings now: it must never
    /// stay a silent fallback (B9-R-R).
    fn observe_confinement_refusal(&mut self, run_dir: &Path) {
        if !self.options.confinement.confined() {
            return;
        }
        let Some(reason) = crate::run::read_confinement_refusal(run_dir) else {
            return;
        };
        self.options.confinement = crate::confinement::Method::None;
        self.note(format!("Runs are not confined on this computer — {reason}"));
    }

    /// The saved choice for one agent, committed through the settings helper.
    fn set_desired(&mut self, index: usize, desired: Desired) -> Result<(), String> {
        let name = self.agents[index].name.clone();
        self.commit_settings(|settings| {
            if let Some(saved) = settings.agent_mut(&name) {
                saved.set_desired(desired);
            }
            Ok(())
        })?;
        self.agents[index].desired = desired;
        Ok(())
    }
}

impl AgentRuntime {
    fn load(inner: &Inner, name: &str) -> Self {
        let saved = inner.settings.agent(name);
        let desired = saved
            .map(AgentSettings::desired)
            .unwrap_or(Desired::Stopped);
        let recorded_login = credentials::recorded_login(&inner.credentials, name)
            .ok()
            .flatten();
        let login = recorded_login
            .clone()
            .or_else(|| identity::login_for(name).ok());
        let email = credentials::recorded_email(&inner.credentials, name)
            .ok()
            .flatten();
        let rights = credentials::recorded_agent_identity_rights(&inner.credentials, name)
            .ok()
            .flatten();
        let token_present = credentials::forgejo_agent_token_optional(&inner.credentials, name)
            .ok()
            .flatten()
            .is_some();
        let provisioning = if recorded_login.is_some()
            && token_present
            && rights.as_deref() == Some(identity::TOOL_RIGHTS_VERSION)
        {
            Provisioning::Ready
        } else {
            Provisioning::Pending
        };
        let paths = inner.options.layout.agent(name).ok();
        let instruction_path = paths
            .as_ref()
            .map(|paths| paths.instruction().to_path_buf());
        let (chars, first_line, instruction_text, lines, mtime) = instruction_path
            .as_deref()
            .map(instruction_facts)
            .unwrap_or((0, None, None, 0, None));
        let mut runs = run::runs_of_agent(inner.options.layout.state_root(), name);
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        let mut recent = VecDeque::new();
        for summary in &runs {
            if let Some(end) = end_from_outcome(summary) {
                recent.push_back(end);
            }
            while recent.len() > 10 {
                recent.pop_front();
            }
        }
        let newest_run = runs.last().map(|summary| summary.dir.clone());
        let activity = newest_run
            .as_deref()
            .and_then(|dir| run::forgejo_actions(dir).pop())
            .map(|action| plain_activity(&action.sentence));
        let activity_at = newest_run
            .as_deref()
            .and_then(|dir| fs::metadata(dir).ok())
            .and_then(|metadata| metadata.modified().ok());
        let last_error = newest_run.as_deref().and_then(|dir| {
            run::runs_of_agent(inner.options.layout.state_root(), name)
                .into_iter()
                .rev()
                .find_map(|summary| {
                    if summary.dir == dir {
                        match summary.outcome {
                            Some(run::RunOutcome::Failed) => summary.reason,
                            _ => None,
                        }
                    } else {
                        None
                    }
                })
        });
        let last_failure = newest_run.as_deref().and_then(run::read_failure);
        // The work block stays until a later run does new work: a read-only check
        // run must not erase the pull request, branch and comment a work run left
        // (W31-3). A run whose own log says nothing was assigned gets the plain
        // empty-work sentence on the Agent screen (F2).
        let last_work = run::latest_work(&runs);
        let last_result = runs.last().and_then(|summary| match summary.outcome {
            Some(
                run::RunOutcome::Succeeded
                | run::RunOutcome::Refused
                | run::RunOutcome::NoWork
                | run::RunOutcome::Warned,
            ) => summary.reason.clone(),
            _ => None,
        });
        let no_assigned_work = newest_run
            .as_deref()
            .is_some_and(|dir| run::says_no_assigned_work(dir, &format!("gewu-{name}")));
        let last_work_result = run::latest_work_result(&runs);
        let last_result_is_work = newest_run.as_deref().is_some_and(run::run_did_work);
        let removal_reason = removal_marked(&inner.options.layout, name).then(|| {
            "its Forgejo sign-in and tokens were already cancelled; Remove can be retried"
                .to_owned()
        });
        let provisioning = match &removal_reason {
            Some(reason) => Provisioning::Failed {
                reason: reason.clone(),
            },
            None => provisioning,
        };
        Self {
            name: name.to_owned(),
            desired,
            provisioning,
            login,
            email,
            session: removal_reason.map_or(SessionState::Idle, |reason| SessionState::Blocked {
                reason,
            }),
            sessions_since_start: 0,
            runs_total: runs.len() as u64,
            session_number: 0,
            recent,
            instruction_chars: chars,
            instruction_first_line: first_line,
            instruction: instruction_text,
            instruction_lines: lines,
            instruction_mtime: mtime,
            activity,
            activity_at,
            last_error,
            last_result,
            last_failure,
            leftover: None,
            leftover_targets: Vec::new(),
            current_tool: None,
            current_tool_since: None,
            retrying: None,
            last_work,
            no_assigned_work,
            last_work_result,
            last_result_is_work,
            activity_refresh_at: None,
            newest_run,
            running: None,
            starting: false,
            starting_pid: None,
            starting_since: None,
            next_at: None,
            last_end: None,
            quick_failures: 0,
            provision_inflight: false,
            provision_retry_at: None,
            removal_pending: false,
            removal_retry_at: None,
            program_hint: None,
        }
    }

    fn refresh_instruction(&mut self, paths: Option<&crate::paths::AgentPaths>) {
        let Some(paths) = paths else {
            return;
        };
        let mtime = fs::metadata(paths.instruction())
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        if mtime == self.instruction_mtime && self.instruction_first_line.is_some() {
            return;
        }
        let (chars, first_line, instruction, lines, mtime) = instruction_facts(paths.instruction());
        self.instruction_chars = chars;
        self.instruction_first_line = first_line;
        self.instruction = instruction;
        self.instruction_lines = lines;
        self.instruction_mtime = mtime;
    }
}

fn end_from_outcome(summary: &run::RunSummary) -> Option<SessionEnd> {
    match summary.outcome? {
        run::RunOutcome::Succeeded => Some(SessionEnd::Succeeded),
        run::RunOutcome::Refused => Some(SessionEnd::Refused {
            reason: crate::forgejo_tools::sentence(
                summary
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the run refused the instruction".to_owned()),
            ),
        }),
        run::RunOutcome::NoWork => Some(SessionEnd::NoWork {
            reason: crate::forgejo_tools::sentence(
                summary
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the run had no work".to_owned()),
            ),
        }),
        run::RunOutcome::Warned => Some(SessionEnd::Warned {
            reason: crate::forgejo_tools::sentence(
                summary
                    .reason
                    .clone()
                    .unwrap_or_else(|| "a command failed".to_owned()),
            ),
        }),
        run::RunOutcome::Failed => Some(SessionEnd::Failed {
            reason: crate::forgejo_tools::sentence(
                summary
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the session failed".to_owned()),
            ),
        }),
        run::RunOutcome::TimedOut => Some(SessionEnd::TimedOut),
        run::RunOutcome::Stopped => Some(SessionEnd::Stopped),
    }
}

/// The ending already written by a run, as the engine's own session ending.
fn end_from_terminal_ending(ending: &run::TerminalEnding) -> Option<SessionEnd> {
    let reason = crate::forgejo_tools::sentence(ending.reason.clone().unwrap_or_default());
    match ending.outcome {
        crate::run::RunOutcome::Succeeded => Some(SessionEnd::Succeeded),
        crate::run::RunOutcome::Refused => Some(SessionEnd::Refused { reason }),
        crate::run::RunOutcome::NoWork => Some(SessionEnd::NoWork { reason }),
        crate::run::RunOutcome::Warned => Some(SessionEnd::Warned { reason }),
        crate::run::RunOutcome::Failed => Some(SessionEnd::Failed { reason }),
        crate::run::RunOutcome::TimedOut => Some(SessionEnd::TimedOut),
        crate::run::RunOutcome::Stopped => Some(SessionEnd::Stopped),
    }
}

/// The durable outcome and reason for one run the engine saw end.
///
/// The engine's own ending is used only when the supervisor did not write one: the
/// transcript keeps the run's own record when it exists. A stop or timeout that the
/// engine caused gets a sentence that says so rather than an empty reason.
fn outcome_for_end(end: &SessionEnd) -> (run::RunOutcome, Option<String>) {
    match end {
        SessionEnd::Succeeded => (run::RunOutcome::Succeeded, None),
        SessionEnd::Refused { reason } => (run::RunOutcome::Refused, Some(reason.clone())),
        SessionEnd::NoWork { reason } => (run::RunOutcome::NoWork, Some(reason.clone())),
        SessionEnd::Warned { reason } => (run::RunOutcome::Warned, Some(reason.clone())),
        SessionEnd::Failed { reason } => (run::RunOutcome::Failed, Some(reason.clone())),
        SessionEnd::TimedOut => (
            run::RunOutcome::TimedOut,
            Some("the run reached its time limit and was stopped".to_owned()),
        ),
        SessionEnd::Stopped => (
            run::RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
        ),
    }
}

fn instruction_facts(
    path: &Path,
) -> (
    usize,
    Option<String>,
    Option<String>,
    usize,
    Option<SystemTime>,
) {
    let mtime = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let Ok(bytes) = fs::read(path) else {
        return (0, None, None, 0, mtime);
    };
    if agent_run::instruction_is_blank(&bytes) {
        return (0, None, None, 0, mtime);
    }
    let text = String::from_utf8_lossy(&bytes);
    let chars = text.chars().count();
    // File lines, not wrapped display rows: the card's `(N more lines)` counts what
    // is written in the document (U4/F-11).
    let lines = text.lines().count().max(1);
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(first_line_at_a_word);
    (chars, first, Some(text.into_owned()), lines, mtime)
}

/// The first line of an instruction, cut at 120 characters on a word.
///
/// A card is read as a sentence. Cutting the stored line at a character count and
/// leaving the renderer to ellipsize what it received made `extraordinary` render as
/// `extraord…` at 80 columns (review B3). The cap is applied here, to the whole
/// first line, and the cut falls back to the last whole word before it.
fn first_line_at_a_word(line: &str) -> String {
    const CAP: usize = 120;
    let full = line.chars().count();
    if full <= CAP {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(CAP).collect();
    if let Some(at) = cut.rfind(char::is_whitespace) {
        cut.truncate(at);
    }
    format!("{}…", cut.trim_end())
}

fn backoff_for_quick_failure(failures: u32) -> Duration {
    let factor = 1u64 << failures.min(6);
    QUICK_FAILURE_DELAY
        .saturating_mul(factor as u32)
        .min(MAX_RESTART_DELAY)
}

/// One piece of slow work to run outside the state lock.
enum Job {
    ForgejoStart,
    ForgejoProvision,
    RepositoryRefresh,
    ProvisionAgent(String),
    Session(Box<SessionRequest>),
    RemoveAgent(String),
}

impl Engine {
    /// The pids this engine started, for a test or a report that must prove every one
    /// of them is gone after `shutdown`. Never used by the board.
    #[doc(hidden)]
    pub fn live_pids(&self) -> Vec<u32> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut pids = Vec::new();
        if let Some(server) = inner.forgejo.child.as_ref() {
            pids.push(server.pid());
        }
        if let Some(pid) = inner.forgejo.start_pid {
            pids.push(pid);
        }
        pids.extend(inner.forgejo.helper_pids.iter().copied());
        for agent in &inner.agents {
            if let Some(run) = agent.running.as_ref() {
                pids.push(run.run.pid());
            }
            if let Some(pid) = agent.starting_pid {
                pids.push(pid);
            }
        }
        pids
    }

    /// The design's tick: reap, start due sessions, follow the Forgejo child.
    fn tick_at(&mut self) {
        let now = self.clock.now();
        let jobs = {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            inner.poll(now);
            inner.due_jobs(now)
        };
        for job in jobs {
            self.run_job(job);
        }
    }

    fn run_job(&self, job: Job) {
        let inner = Arc::clone(&self.inner);
        let clock = Arc::clone(&self.clock);
        let spawner = Arc::clone(&self.spawner);
        let generation = inner
            .lock()
            .map(|inner| inner.generation)
            .unwrap_or_default();
        match job {
            Job::ForgejoStart => {
                let options = self.options.clone();
                std::thread::spawn(move || forgejo_start_job(inner, options, generation));
            }
            Job::ForgejoProvision => {
                let options = self.options.clone();
                std::thread::spawn(move || forgejo_provision_job(inner, options));
            }
            Job::RepositoryRefresh => {
                std::thread::spawn(move || repository_refresh_job(inner));
            }
            Job::ProvisionAgent(name) => {
                std::thread::spawn(move || provision_agent_job(inner, name));
            }
            Job::Session(request) => {
                std::thread::spawn(move || {
                    session_job(inner, *request, spawner, clock, generation);
                });
            }
            Job::RemoveAgent(name) => {
                std::thread::spawn(move || remove_agent_job(inner, name));
            }
        }
    }
}

impl Inner {
    fn poll(&mut self, now: Instant) {
        if self.stopping {
            return;
        }
        self.refresh_instructions();
        self.poll_forgejo(now);
        for index in 0..self.agents.len() {
            self.poll_agent(index, now);
        }
    }

    fn refresh_instructions(&mut self) {
        for index in 0..self.agents.len() {
            let paths = self.options.layout.agent(&self.agents[index].name).ok();
            self.agents[index].refresh_instruction(paths.as_ref());
        }
    }

    fn poll_forgejo(&mut self, now: Instant) {
        #[cfg(test)]
        if self.options.forgejo_ready {
            self.forgejo.state = ForgejoState::Running;
            return;
        }
        if let Some(server) = self.forgejo.child.as_mut() {
            if server.try_status().is_some() {
                self.forgejo.child = None;
                self.forgejo.state = ForgejoState::Failed {
                    reason: "Forgejo stopped unexpectedly — restarting it".to_owned(),
                };
                self.forgejo.provisioned = false;
                self.forgejo.admin_token = None;
                self.forgejo.failures = self.forgejo.failures.saturating_add(1);
                let delay = backoff_for_restart(self.forgejo.failures);
                self.forgejo.retry_at = Some(now + delay);
                self.forgejo.retry_start = true;
                self.forgejo.start_requested = true;
                self.note("Forgejo stopped unexpectedly — restarting it");
            } else if server.uptime() >= LONG_SESSION {
                self.forgejo.failures = 0;
            }
        }
        if !self.forgejo.start_inflight
            && self.forgejo.child.is_none()
            && self.forgejo.start_requested
            && self.forgejo.retry_at.is_none_or(|retry_at| now >= retry_at)
        {
            // `due_jobs` turns this into the actual start.
        }
    }

    fn poll_agent(&mut self, index: usize, now: Instant) {
        let desired = self.agents[index].desired;
        let mut ended: Option<EndedRun> = None;
        if let Some(run) = self.agents[index].running.as_mut() {
            if !run.stopping {
                if now.saturating_duration_since(run.started) >= self.options.session_timeout {
                    let proposed = proposed_first_ending(run);
                    let requested = run.run.request_stop(proposed);
                    match requested {
                        Ok(ending) => {
                            run.first_ending = Some(ending);
                            run.first_ending_error = None;
                            run.timed_out = true;
                            run.stopping = true;
                            run.stop_deadline = Some(now + self.options.stop_grace);
                        }
                        Err(error) => {
                            // The owner recorded the uncertainty; do not signal a run
                            // whose first ending is not durable.
                            if run.first_ending_error.is_none() {
                                run.first_ending_error = Some(error);
                            }
                        }
                    }
                }
            } else if run.stop_deadline.is_some_and(|deadline| now >= deadline) {
                let _ = run.run.request_force("the run reached its force deadline");
                run.stop_deadline = Some(now + Duration::from_secs(1));
            }
            if let Some(status) = run.run.try_status() {
                let engine_stopped = desired == Desired::Stopped || run.stop_requested;
                let timed_out = run.timed_out;
                let first_ending = run.first_ending;
                // A run that had already written its ending when the first stop was
                // requested won the race. The engine's own stop is authoritative
                // only when the write-once fact says Stop.
                let stop_wins = (engine_stopped || timed_out)
                    && !matches!(first_ending, Some(run::FirstEnding::Terminal));
                let terminal = run::terminal_ending(&run.run_dir).ok().flatten();
                let progress = if timed_out && stop_wins {
                    run::native_progress(&run.run_dir)
                } else {
                    None
                };
                let deadline_end = || {
                    // A harness that reported its own failure before the engine's
                    // deadline is not only "timed out": the last cause survives the
                    // kill and supplies a failed ending with a reason.
                    match progress.as_ref() {
                        Some(progress)
                            if !progress.cause.trim().is_empty()
                                && matches!(progress.state.as_str(), "failed" | "retrying") =>
                        {
                            SessionEnd::Failed {
                                reason: crate::forgejo_tools::sentence(format!(
                                    "the run reached its time limit with a reported failure: {}",
                                    progress.cause
                                )),
                            }
                        }
                        _ => SessionEnd::TimedOut,
                    }
                };
                let end = if stop_wins {
                    if engine_stopped {
                        SessionEnd::Stopped
                    } else {
                        deadline_end()
                    }
                } else if let Some(record) = terminal.as_ref() {
                    end_from_terminal_ending(record).unwrap_or_else(|| {
                        if status.success() {
                            session_success_end(&run.run_dir)
                        } else {
                            SessionEnd::Failed {
                                reason: session_failure_reason(&run.run_dir, status),
                            }
                        }
                    })
                } else if engine_stopped {
                    SessionEnd::Stopped
                } else if timed_out {
                    deadline_end()
                } else if status.success() {
                    // Exit 0 is not the whole outcome: the run's own terminal record
                    // says whether its last sentence refused the instruction or found
                    // no work (F4). The record is written before the process exits.
                    session_success_end(&run.run_dir)
                } else {
                    SessionEnd::Failed {
                        reason: session_failure_reason(&run.run_dir, status),
                    }
                };
                ended = Some((
                    end,
                    run.started,
                    run.run_dir.clone(),
                    run.run.pid(),
                    stop_wins,
                    progress,
                    first_ending,
                ));
            }
        }
        if let Some(agent) = self.agents.get(index)
            && let Some(error) = agent
                .running
                .as_ref()
                .and_then(|run| run.first_ending_error.clone())
        {
            self.note(format!("the deadline stop cannot be recorded: {error}"));
        }
        if let Some((mut end, started, run_dir, pid, stop_wins, progress, first_ending)) = ended {
            self.observe_confinement_refusal(&run_dir);
            // A supervisor killed from outside can leave the shell it started in
            // the run's process group. The boundary settles the run in this order:
            // stop or find the group gone, write (or keep) a durable ending, and
            // only then remove the identity handles. An error at either step keeps
            // the handles and is reported on the card; Start retries it (F-02).
            let (outcome, mut reason) = outcome_for_end(&end);
            // The engine's own deadline must carry the last cause the harness
            // emitted: the child is already gone, so the persisted progress record
            // is the only surviving source.
            if let Some(progress) = progress.as_ref() {
                if !progress.cause.trim().is_empty() {
                    let base = reason
                        .clone()
                        .unwrap_or_else(|| "the run was ended by the product".to_owned());
                    if !base.contains(&progress.cause) {
                        reason = Some(format!("{base}; last reported cause: {}", progress.cause));
                    }
                }
                // A failed tool is a warning attached to the run, never a
                // session-level failure cause: it is named here but cannot turn a
                // deadline into `failed`.
                if let Some(warning) = progress.warning.as_deref().filter(|w| !w.trim().is_empty())
                {
                    let base = reason
                        .clone()
                        .unwrap_or_else(|| "the run was ended by the product".to_owned());
                    if !base.contains(warning) {
                        reason = Some(format!("{base}; tool warning: {warning}"));
                    }
                }
            }
            let stop_reason = if stop_wins { reason.clone() } else { None };
            let settled = run::request_finish(
                &run_dir,
                pid,
                outcome,
                reason,
                self.options.stop_grace,
                stop_reason,
                first_ending.unwrap_or(if stop_wins {
                    run::FirstEnding::Stop
                } else {
                    run::FirstEnding::Terminal
                }),
            );
            self.agents[index].leftover = settled.err();
            if self.agents[index].leftover.is_some() {
                self.agents[index].leftover_targets = vec![RecoveryTarget::Run(run_dir.clone())];
            } else {
                self.agents[index].leftover_targets.clear();
            }
            // The settle may have replaced a child's post-stop ending with the
            // engine's stop ending (or preserved the child's earlier ending). Read
            // the one terminal record back and use it for the board, so the card and
            // the transcript cannot disagree.
            if let Some(record) = run::terminal_ending(&run_dir).ok().flatten()
                && let Some(actual) = end_from_terminal_ending(&record)
            {
                end = actual;
            }
            // A stopped, killed or crashed run gets no chance to run its own
            // destructor: the engine removes its private askpass tree here (F-01).
            if let Some(run_id) = run_id_in(&run_dir) {
                crate::confinement::remove_run_temp(pid, run_id);
            }
            self.agents[index].running = None;
            self.agents[index].starting = false;
            self.agents[index].activity_refresh_at = None;
            let state_root = self.options.layout.state_root().to_path_buf();
            let agent = &mut self.agents[index];
            finish_session(agent, end, now, started, &state_root, Some(&run_dir));
            if agent.leftover.is_some() {
                // A surviving old process outranks the normal restart: block the
                // agent (and say why) until Start retries the cleanup.
                agent.session = SessionState::Blocked {
                    reason: agent
                        .leftover
                        .clone()
                        .unwrap_or_else(|| "cleanup unverified".to_owned()),
                };
                agent.next_at = None;
            }
        } else if let Some(run_dir) = self.agents[index]
            .running
            .as_ref()
            .map(|run| run.run_dir.clone())
        {
            // While a session runs, its newest action is read from its own transcript
            // at most once a second, so the card's activity is current without doing
            // file work on every frame.
            if self.agents[index]
                .activity_refresh_at
                .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
            {
                if let Some(action) = run::forgejo_actions(&run_dir).pop() {
                    self.agents[index].activity = Some(plain_activity(&action.sentence));
                    self.agents[index].activity_at = Some(SystemTime::now());
                }
                self.agents[index].current_tool =
                    run::read_current_tool(&run_dir).map(|(tool, _started)| tool);
                self.agents[index].current_tool_since =
                    run::read_current_tool(&run_dir).map(|(_tool, started)| started);
                if let Some(failure) = run::read_failure(&run_dir) {
                    self.agents[index].last_failure = Some(failure);
                }
                // A native program retries a temporary provider failure inside its
                // own turn. The run is still working, but the card must not stay
                // silent about a 503 while it waits and retries.
                self.agents[index].retrying = run::native_progress(&run_dir)
                    .filter(|progress| {
                        progress.state == "retrying"
                            && !progress.terminal
                            && temporary_model_cause(&progress.cause)
                    })
                    .map(|progress| progress.cause);
                self.observe_confinement_refusal(&run_dir);
                self.agents[index].activity_refresh_at = Some(now);
            }
        }
        if self.agents[index].removal_pending {
            return;
        }
        if self.agents[index].desired == Desired::Stopped {
            if self.agents[index].running.is_none() {
                self.agents[index].session = SessionState::Idle;
                self.agents[index].next_at = None;
                self.agents[index].starting = false;
            }
            return;
        }
        if self.agents[index].running.is_some() || self.agents[index].starting {
            return;
        }
        if let Some(reason) = self.block_reason(index) {
            if !matches!(
                &self.agents[index].session,
                SessionState::Blocked { reason: current } if current == &reason
            ) {
                self.agents[index].session = SessionState::Blocked { reason };
            }
            self.agents[index].next_at = None;
            return;
        }
        if matches!(self.agents[index].session, SessionState::Blocked { .. }) {
            self.agents[index].next_at = Some(now);
            let last = self.agents[index]
                .last_end
                .clone()
                .unwrap_or(SessionEnd::Succeeded);
            self.agents[index].session = SessionState::Restarting {
                next_at: SystemTime::now(),
                last,
            };
        }
    }

    fn block_reason(&self, index: usize) -> Option<String> {
        let agent = &self.agents[index];
        if agent.leftover.is_some() {
            // Never start a replacement beside a process the last cleanup could
            // not stop; the card says why and Start retries the cleanup.
            return agent
                .leftover
                .clone()
                .or_else(|| Some("cleanup unverified".to_owned()));
        }
        if let Some(reason) = self.program_block(agent) {
            return Some(reason);
        }
        if agent.instruction_chars == 0 {
            return Some("needs an instruction".to_owned());
        }
        #[cfg(test)]
        if self.options.forgejo_ready {
            return self.account_block(agent);
        }
        match &self.forgejo.state {
            ForgejoState::Running => {}
            _ => return Some("waiting for Forgejo".to_owned()),
        }
        // Running is not enough: the first run after a reopen or a recovery must
        // carry the organisation's repositories and the six Forgejo tools, and those
        // arrive only when the provisioning job has returned them. A run started in
        // the gap between "running" and "provisioned" would be shell-only.
        if !self.forgejo.provisioned || self.forgejo.repositories.is_empty() {
            return Some("waiting for Forgejo".to_owned());
        }
        self.account_block(agent)
    }

    fn account_block(&self, agent: &AgentRuntime) -> Option<String> {
        match &agent.provisioning {
            Provisioning::Ready => None,
            Provisioning::Pending | Provisioning::Failed { .. } => {
                Some("account not ready".to_owned())
            }
        }
    }

    fn due_jobs(&mut self, now: Instant) -> Vec<Job> {
        let mut jobs = Vec::new();
        if self.stopping {
            return jobs;
        }
        #[cfg(test)]
        let simulated_ready = self.options.forgejo_ready;
        #[cfg(not(test))]
        let simulated_ready = false;
        if !simulated_ready
            && !self.forgejo.start_inflight
            && self.forgejo.child.is_none()
            && self.forgejo.start_requested
            && self.forgejo.retry_at.is_none_or(|retry_at| now >= retry_at)
        {
            self.forgejo.start_inflight = true;
            self.forgejo.start_requested = false;
            self.forgejo.retry_at = None;
            self.forgejo.state = if bundled_forgejo::binary_path(&self.options.data_root).is_file()
            {
                ForgejoState::Preparing
            } else {
                ForgejoState::Fetching { percent: None }
            };
            jobs.push(Job::ForgejoStart);
        }
        if !simulated_ready
            && matches!(self.forgejo.state, ForgejoState::Running)
            && !self.forgejo.provision_inflight
            && !self.forgejo.provisioned
            && self
                .forgejo
                .provision_retry_at
                .is_none_or(|retry_at| now >= retry_at)
        {
            // The job can make the administrator token itself; requiring one here
            // would leave a server whose first token attempt failed retrying nothing.
            self.forgejo.provision_inflight = true;
            jobs.push(Job::ForgejoProvision);
        }
        if !simulated_ready
            && matches!(self.forgejo.state, ForgejoState::Running)
            && self.forgejo.admin_token.is_some()
            && self.forgejo.provisioned
            && !self.forgejo.repositories_inflight
            && self
                .forgejo
                .repositories_refreshed_at
                .is_none_or(|at| now.saturating_duration_since(at) >= REPOSITORY_REFRESH)
        {
            self.forgejo.repositories_inflight = true;
            jobs.push(Job::RepositoryRefresh);
        }
        if !simulated_ready
            && matches!(self.forgejo.state, ForgejoState::Running)
            && self.forgejo.provisioned
            && self.forgejo.admin_token.is_some()
        {
            for index in 0..self.agents.len() {
                let (skip, name) = {
                    let agent = &self.agents[index];
                    (
                        agent.removal_pending
                            || removal_marked(&self.options.layout, &agent.name)
                            || agent.provisioning == Provisioning::Ready
                            || agent.provision_inflight
                            || agent
                                .provision_retry_at
                                .is_some_and(|retry_at| now < retry_at),
                        agent.name.clone(),
                    )
                };
                if skip {
                    continue;
                }
                self.agents[index].provision_inflight = true;
                jobs.push(Job::ProvisionAgent(name));
            }
        }
        for index in 0..self.agents.len() {
            let (removal, name, ready, due) = {
                let agent = &self.agents[index];
                (
                    agent.removal_pending,
                    agent.name.clone(),
                    agent.desired == Desired::Started
                        && agent.running.is_none()
                        && !agent.starting
                        && !matches!(agent.session, SessionState::Blocked { .. })
                        && agent.next_at.is_none_or(|next_at| now >= next_at),
                    agent
                        .removal_retry_at
                        .is_none_or(|retry_at| now >= retry_at),
                )
            };
            if removal {
                if self.agents[index].running.is_none()
                    && !self.agents[index].starting
                    && !self.agents[index].provision_inflight
                    && due
                {
                    jobs.push(Job::RemoveAgent(name));
                }
                continue;
            }
            if !ready {
                continue;
            }
            if self.block_reason(index).is_some() {
                continue;
            }
            if let Some(request) = self.session_request(index) {
                self.agents[index].starting = true;
                self.agents[index].starting_since = Some(now);
                self.agents[index].session_number =
                    self.agents[index].session_number.saturating_add(1);
                self.agents[index].runs_total = self.agents[index].runs_total.saturating_add(1);
                self.agents[index].sessions_since_start =
                    self.agents[index].sessions_since_start.saturating_add(1);
                self.agents[index].session = SessionState::Running {
                    number: self.agents[index].session_number,
                    since: SystemTime::now(),
                };
                jobs.push(Job::Session(Box::new(request)));
            }
        }
        jobs
    }

    fn session_request(&mut self, index: usize) -> Option<SessionRequest> {
        let agent_name = self.agents[index].name.clone();
        let paths = self.options.layout.agent(&agent_name).ok()?;
        let workspace = paths.workspace().to_path_buf();
        let hide = vec![
            self.options.layout.root().to_path_buf(),
            self.options.layout.state_root().to_path_buf(),
            self.options.data_root.clone(),
        ];
        // The layout may have changed since the engine opened (a workspace turned
        // into a symlink, a root moved). Check the actual run-time layout here, in
        // the parent, and take the result onto the board before the plan is written:
        // a run must not start with a promise `Shell::prepare` will refuse (B9-R-R).
        let real_home = std::env::var_os("HOME").map(PathBuf::from);
        let run_hide = crate::confinement::run_hide_roots(&hide, real_home.as_deref());
        let requested = self.options.confinement;
        let (confinement, refusal) =
            crate::confinement::effective_method(requested, &run_hide, &workspace);
        if confinement != requested {
            self.options.confinement = confinement;
            if !confinement.confined() {
                self.note(format!(
                    "Runs are not confined on this computer — {}",
                    refusal.unwrap_or("the layout changed")
                ));
            }
        }
        let agent = &self.agents[index];
        let saved_agent = self.settings.agent(&agent_name).cloned()?;
        let program = saved_agent.harness();
        let (protocol, base_url, model) = self.connection_for(&saved_agent)?;
        let native = if program.is_native() {
            let installed = harness::installed(&self.options.data_root, program)?;
            Some(NativeRequest {
                harness: program.id().to_owned(),
                version: installed.version,
                program: installed.executable,
                install_root: installed.root,
                protocol: protocol.id().to_owned(),
                base_url: base_url.clone(),
                model: model.clone(),
                codex_sandbox: saved_agent.codex_sandbox.clone(),
            })
        } else {
            None
        };
        self.sequence = self.sequence.saturating_add(1);
        let run_id = run::new_run_id(self.sequence);
        let run_dir = paths.run_dir(&run_id);
        let login = agent.login.clone()?;
        let email = agent
            .email
            .clone()
            .unwrap_or_else(|| identity::email_for(&login));
        let repositories = self
            .forgejo
            .repositories
            .iter()
            .take(20)
            .map(|repository| SessionRepository {
                directory: repository.name.clone(),
                full_name: repository.full_name.clone(),
            })
            .collect();
        Some(SessionRequest {
            agent: agent_name,
            run_id,
            run_dir,
            state_root: self.options.layout.state_root().to_path_buf(),
            workspace,
            instruction: paths.instruction().to_path_buf(),
            skills_dir: paths.skills_dir().to_path_buf(),
            endpoint: if native.is_none() {
                self.settings.endpoint.chat_url()
            } else {
                base_url.clone()
            },
            model: model.clone(),
            max_turns: self.settings.endpoint.turns(),
            timeout_secs: self.options.session_timeout.as_secs(),
            credentials: self.credentials.clone(),
            login,
            email,
            forgejo_url: self.forgejo.url.clone(),
            repositories,
            confinement,
            hide,
            native,
        })
    }

    /// The selected connection for one agent: its explicit provider for a native
    /// program, or the shared endpoint for the legacy built-in engine.
    fn connection_for(
        &self,
        agent: &settings::AgentSettings,
    ) -> Option<(Protocol, String, String)> {
        if agent.harness().is_native() {
            let provider = agent.provider()?;
            if !provider.configured() {
                return None;
            }
            Some((
                provider.protocol(),
                provider.base_url.trim_end_matches('/').to_owned(),
                provider.model.clone(),
            ))
        } else {
            if !self.settings.endpoint.configured() {
                return None;
            }
            Some((
                self.settings.endpoint.protocol(),
                self.settings.endpoint.base_url(),
                self.settings.endpoint.model.clone(),
            ))
        }
    }

    /// Probe Codex's own bundled bubblewrap before a default-sandbox Start. Codex
    /// 0.164 does not emit a native event when this setup fails, so Start would
    /// otherwise show only a later downstream refusal. The probe uses no secrets
    /// and inherits the product's existing outer Landlock.
    fn codex_sandbox_block(&self, agent: &AgentRuntime) -> Option<String> {
        let saved = self.settings.agent(&agent.name)?;
        if saved.harness() != Program::Codex || saved.codex_sandbox == "full-access" {
            return None;
        }
        let installed = harness::installed(&self.options.data_root, Program::Codex)?;
        let bwrap = installed.root.join("codex-resources").join("bwrap");
        if !bwrap.is_file() {
            return Some("the native sandbox helper is missing".to_owned());
        }
        let mut child = match std::process::Command::new(&bwrap)
            .args([
                "--unshare-user",
                "--unshare-net",
                "--ro-bind",
                "/",
                "/",
                "/bin/true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return Some(format!(
                    "the native sandbox helper could not start: {error}"
                ));
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some(
                        "the native sandbox preflight did not finish within 2 seconds".to_owned(),
                    );
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some(format!(
                        "the native sandbox preflight could not be read: {error}"
                    ));
                }
            }
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        if status.is_some_and(|status| status.success()) {
            return None;
        }
        let detail = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("the native sandbox boundary refused its probe")
            .trim();
        Some(detail.to_owned())
    }

    /// Why one agent's selected program/connection cannot start, if it cannot.
    fn program_block(&self, agent: &AgentRuntime) -> Option<String> {
        let saved = self.settings.agent(&agent.name)?;
        if let Some(unknown) = saved.unknown_harness() {
            return Some(format!("unknown saved program `{unknown}`"));
        }
        let program = saved.harness();
        if program == Program::Builtin {
            if !self.settings.endpoint.configured() {
                return Some("needs a model".to_owned());
            }
            if credentials::model_key(&self.credentials, &agent.name).is_err() {
                return Some("needs a model key".to_owned());
            }
            return None;
        }
        let Some(installed) = harness::installed(&self.options.data_root, program) else {
            return Some(format!("{} is not installed", program.label()));
        };
        let Some((protocol, _base, model)) = self.connection_for(saved) else {
            return Some("choose this agent's connection and model".to_owned());
        };
        if model.trim().is_empty() {
            return Some("choose this agent's model".to_owned());
        }
        if let Some(error) = harness::compatibility_error(program, protocol) {
            return Some(error);
        }
        if credentials::model_key(&self.credentials, &agent.name).is_err() {
            return Some("needs a model key".to_owned());
        }
        if program == Program::Dsh {
            if let Some(prerequisite) = program.prerequisite_missing() {
                return Some(prerequisite_action(prerequisite).to_owned());
            }
        }
        if installed.program != program {
            return Some("the installed program is for another program".to_owned());
        }
        None
    }

    /// The installed-state cards for the Program screen.
    fn program_views(&self) -> Vec<ProgramView> {
        harness::NATIVE_PROGRAMS
            .into_iter()
            .map(|program| {
                let installed = harness::installed(&self.options.data_root, program);
                let state = self.program_states.get(program.id());
                ProgramView {
                    id: program.id().to_owned(),
                    label: program.label().to_owned(),
                    version: program.pinned_version().to_owned(),
                    installed_version: installed.as_ref().map(|item| item.version.clone()),
                    installed: installed.is_some(),
                    license: program.license().to_owned(),
                    download_mb: program.download_mb(),
                    prerequisite: program.prerequisite().map(str::to_owned),
                    prerequisite_met: program.prerequisite_missing().is_none(),
                    installing: state.and_then(|state| state.installing),
                    installing_phase: state.and_then(|state| {
                        let phase = state.phase?;
                        let elapsed = state
                            .started
                            .map(|started| started.elapsed().as_secs())
                            .unwrap_or(0);
                        Some(if elapsed > 0 {
                            format!("{} · {elapsed}s", phase.label())
                        } else {
                            phase.label().to_owned()
                        })
                    }),
                    error: state.and_then(|state| state.error.clone()),
                }
            })
            .collect()
    }

    /// Give a newly created agent the installation's default program on a settings
    /// copy. The caller commits the copy and writes the returned inherited key.
    fn assign_new_agent_program(
        &self,
        settings: &mut settings::Settings,
        name: &str,
    ) -> Result<Option<String>, String> {
        let program = settings
            .default_harness
            .as_deref()
            .and_then(Program::parse)
            .unwrap_or(Program::Builtin);
        let endpoint = settings.endpoint.clone();
        let key = credentials::model_key(&self.credentials, name).ok();
        let explicit_key = credentials::agent_model_key(&self.credentials, name)
            .ok()
            .flatten()
            .is_some();
        let Some(agent) = settings.agent_mut(name) else {
            return Ok(None);
        };
        agent.set_harness(program);
        if program.is_native() && agent.provider.is_none() && endpoint.configured() {
            agent.provider = Some(settings::AgentProvider {
                base_url: endpoint.base_url(),
                protocol: endpoint.protocol().id().to_owned(),
                model: endpoint.model.clone(),
            });
        }
        if program == Program::Codex {
            agent.codex_sandbox = "native".to_owned();
        }
        if program.is_native() && !explicit_key {
            Ok(key.map(|key| key.expose().to_owned()))
        } else {
            Ok(None)
        }
    }

    /// The immutable program facts of the newest run, when there is one.
    fn active_program(&self, agent: &AgentRuntime) -> Option<RunProgramView> {
        let dir = agent.newest_run.as_deref()?;
        if let Ok(Some(native)) = crate::run::NativePlan::read(dir) {
            let program = Program::parse(&native.harness).unwrap_or(Program::Builtin);
            return Some(RunProgramView {
                program: native.harness.clone(),
                label: program.label().to_owned(),
                version: native.version,
                protocol: native.protocol,
                base_url: native.base_url,
                model: native.model,
                codex_sandbox: (!native.codex_sandbox.is_empty()).then_some(native.codex_sandbox),
            });
        }
        let plan = run::RunPlan::read(dir).ok()?;
        Some(RunProgramView {
            program: "builtin".to_owned(),
            label: Program::Builtin.label().to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: Protocol::ChatCompletions.id().to_owned(),
            base_url: plan.endpoint,
            model: plan.model,
            codex_sandbox: None,
        })
    }

    /// The saved facts the next run of one agent will use.
    fn next_program(&self, agent: &AgentRuntime) -> Option<RunProgramView> {
        let saved = self.settings.agent(&agent.name)?;
        let program = saved.harness();
        let (protocol, _, model) = self.connection_for(saved).unwrap_or((
            Protocol::ChatCompletions,
            String::new(),
            String::new(),
        ));
        Some(RunProgramView {
            program: program.id().to_owned(),
            label: program.label().to_owned(),
            version: if program == Program::Builtin {
                "always ready".to_owned()
            } else {
                harness::installed(&self.options.data_root, program)
                    .map(|installed| installed.version)
                    .unwrap_or_else(|| "not installed".to_owned())
            },
            protocol: protocol.id().to_owned(),
            base_url: self
                .connection_for(saved)
                .map(|(_, base, _)| base)
                .unwrap_or_default(),
            model,
            codex_sandbox: (program == Program::Codex).then(|| {
                if saved.codex_sandbox == "full-access" {
                    "full-access".to_owned()
                } else {
                    "native".to_owned()
                }
            }),
        })
    }
}

/// The persistent line and exact action for Codex's unavailable own sandbox. The
/// Program screen removes it with `f` twice; the Agent screen names the same key and
/// what it gives up so a person does not have to find the hidden escape from the
/// faded failure alone.
fn codex_sandbox_action(detail: &str) -> String {
    format!(
        "Codex's sandbox cannot start ({detail}) — press f twice: removes Codex's own sandbox, keeps product Landlock"
    )
}

/// Why a native program's environment prerequisite cannot be met, with the exact
/// versions and the key that retries the install. The selected program card already
/// showed the short `needs` line; this is the action that replaces it on failure.
fn prerequisite_action(_missing: &str) -> &'static str {
    "DeepSeek Harness cannot start: install Node.js 24.9+ (npm 11.16+), then press i"
}

fn backoff_for_restart(failures: u32) -> Duration {
    let factor = 1u64 << failures.saturating_sub(1).min(5);
    FORGEJO_RESTART_BASE
        .saturating_mul(factor as u32)
        .min(FORGEJO_RESTART_MAX)
}

fn session_failure_reason(run_dir: &Path, status: std::process::ExitStatus) -> String {
    // The run's own terminal record holds the cause it ended with (a model transport
    // failure, a tool's refusal, a declared step's output). The log's last line is
    // the process-exit line the product itself appends, so it is never the reason
    // (W31-7, F3).
    if let Some(ending) = run::terminal_ending(run_dir).ok().flatten()
        && let Some(reason) = ending.reason
    {
        let reason = crate::forgejo_tools::sentence(reason);
        if !reason.is_empty() {
            return reason;
        }
    }
    if status.signal().is_some() {
        return "run was killed".to_owned();
    }
    if let Some(sentence) = last_meaningful_log_line(run_dir) {
        return sentence;
    }
    match status.code() {
        Some(code) => format!("the run exited with code {code}"),
        None => "the run was ended by a signal".to_owned(),
    }
}

/// The ending a cleanly exited run recorded for itself.
fn session_success_end(run_dir: &Path) -> SessionEnd {
    if let Some(ending) = run::terminal_ending(run_dir).ok().flatten() {
        let reason = crate::forgejo_tools::sentence(ending.reason.unwrap_or_default());
        match ending.outcome {
            crate::run::RunOutcome::Refused => return SessionEnd::Refused { reason },
            crate::run::RunOutcome::NoWork => return SessionEnd::NoWork { reason },
            crate::run::RunOutcome::Warned => return SessionEnd::Warned { reason },
            _ => {}
        }
    }
    SessionEnd::Succeeded
}

/// The last log line that carries a cause, skipping the product's own progress and
/// process-exit lines. A command echo (`[turn 1 tool 1] sleep 60`) is not a reason.
fn last_meaningful_log_line(run_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(run_dir.join(LOG_FILE)).ok()?;
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let cause = |line: &str| {
        let lower = line.to_ascii_lowercase();
        line.starts_with('✗')
            || lower.contains("failed")
            || lower.contains("refused")
            || lower.contains("cannot")
            || lower.contains("could not")
            || lower.contains("error")
            || lower.contains("permission denied")
            || lower.contains("timed out")
    };
    if let Some(line) = lines.iter().rev().find(|line| cause(line)) {
        return Some(crate::forgejo_tools::sentence((*line).to_owned()));
    }
    let line = lines.iter().rev().find(|line| {
        !line.starts_with("agent process exiting")
            && !line.starts_with("[turn ")
            && !line.starts_with("[step ")
            && !line.starts_with("run ")
            && !line.starts_with("workspace:")
            && !line.starts_with("model:")
            && !line.starts_with("instruction read:")
            && !line.starts_with("forgejo tools:")
            && !line.starts_with("skills installed:")
            && !line.starts_with("confinement:")
    })?;
    Some(crate::forgejo_tools::sentence((*line).to_owned()))
}

/// One Forgejo action in the words the card uses: `commented on #12`,
/// `opened pull request #4`, `updated pull request #4`.
fn plain_activity(sentence: &str) -> String {
    let sentence = sentence.trim();
    if let Some(rest) = sentence.strip_prefix("commented on ") {
        let number = rest
            .rsplit_once('#')
            .map(|(_, number)| number)
            .unwrap_or(rest);
        return format!("commented on #{number}");
    }
    if let Some(rest) = sentence.strip_prefix("commented") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return format!("commented on {rest}");
        }
    }
    if let Some(rest) = sentence.strip_prefix("opened ") {
        return format!("opened pull request {}", rest.trim());
    }
    if let Some(rest) = sentence.strip_prefix("updated ") {
        return format!("updated pull request {}", rest.trim());
    }
    sentence.to_owned()
}

/// Whether a provider cause is a temporary condition the program itself retries.
/// A refused key or a missing model is not temporary and is never shown with the
/// retry sentence.
fn temporary_model_cause(cause: &str) -> bool {
    let lower = cause.to_ascii_lowercase();
    if [
        "temporarily unavailable",
        "rate limit",
        "too many requests",
        "overloaded",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return true;
    }
    ["429", "500", "502", "503", "504"]
        .iter()
        .any(|code| lower.contains(code))
}

fn finish_session(
    agent: &mut AgentRuntime,
    end: SessionEnd,
    now: Instant,
    started: Instant,
    state_root: &Path,
    attempt_dir: Option<&Path>,
) {
    // The run's own ending is used even when the user has just pressed Stop: the
    // selector already chose Stopped only when the stop genuinely ended the run.
    // A stop after a terminal record changes the desired state, not this ending.
    let ran = now.saturating_duration_since(started);
    agent.recent.push_back(end.clone());
    while agent.recent.len() > 10 {
        agent.recent.pop_front();
    }
    agent.last_end = Some(end.clone());
    agent.last_error = match &end {
        SessionEnd::Failed { reason } => Some(reason.clone()),
        _ => None,
    };
    let mut result = match &end {
        SessionEnd::Refused { reason }
        | SessionEnd::NoWork { reason }
        | SessionEnd::Warned { reason } => Some(crate::forgejo_tools::sentence(reason.clone())),
        _ => None,
    };
    agent.current_tool = None;
    agent.current_tool_since = None;
    agent.retrying = None;
    // The attempt that just ended owns the failure/activity rows. A prepare or
    // spawn failure has a run directory of its own but never became `newest_run`;
    // reading the previous newest run here showed its old tool failure beside the
    // current start failure and re-stamped its activity as "now". The previous
    // last-work block is still rebuilt from all runs below.
    if let Some(dir) = attempt_dir {
        if let Some(action) = run::forgejo_actions(dir).pop() {
            agent.activity = Some(plain_activity(&action.sentence));
            agent.activity_at = Some(SystemTime::now());
        }
        agent.last_failure = run::read_failure(dir);
        let mut runs = run::runs_of_agent(state_root, &agent.name);
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        agent.last_work = run::latest_work(&runs);
        agent.last_work_result = run::latest_work_result(&runs);
        agent.last_result_is_work = run::run_did_work(dir);
        agent.no_assigned_work = run::says_no_assigned_work(
            dir,
            &agent
                .login
                .clone()
                .unwrap_or_else(|| format!("gewu-{}", agent.name)),
        );
        // A clean success keeps the model's own final summary from its terminal
        // record, so `✓` carries the result row too (review B2). The record is
        // written before the process exits; this reads the same row the reopen
        // reads at startup.
        if matches!(&end, SessionEnd::Succeeded | SessionEnd::Warned { .. }) {
            result = runs
                .iter()
                .rev()
                .find(|summary| summary.dir == dir)
                .and_then(|summary| summary.reason.clone())
                .map(crate::forgejo_tools::sentence);
        }
    }
    agent.last_result = result;
    if agent.desired == Desired::Stopped || end == SessionEnd::Stopped {
        agent.session = SessionState::Idle;
        agent.next_at = None;
        return;
    }
    // A no-work run succeeded at what it was asked to do: it resets the quick-failure
    // streak and takes the documented 20 s watch interval. A warned run did not
    // verify its work, so it takes the same watch interval rather than the five-second
    // success loop that used to replay a deterministic comment every few seconds.
    // Only an actual quick failure grows the back-off, and a successful work run
    // keeps the existing good-run interval.
    let delay = if matches!(end, SessionEnd::NoWork { .. } | SessionEnd::Warned { .. }) {
        agent.quick_failures = 0;
        NO_WORK_DELAY
    } else if matches!(end, SessionEnd::Succeeded) || ran >= LONG_SESSION {
        agent.quick_failures = 0;
        RESTART_DELAY
    } else {
        let delay = backoff_for_quick_failure(agent.quick_failures);
        agent.quick_failures = agent.quick_failures.saturating_add(1);
        delay
    };
    agent.next_at = Some(now + delay);
    agent.session = SessionState::Restarting {
        next_at: SystemTime::now() + delay,
        last: end,
    };
}

impl EngineApi for Engine {
    fn board(&mut self) -> BoardView {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let notice = inner.take_notice();
        let admin_password = inner.admin_password_once.take();
        let endpoint = EndpointView {
            configured: inner.settings.endpoint.configured(),
            url: inner.settings.endpoint.url.clone(),
            base_url: inner.settings.endpoint.base_url(),
            protocol: inner.settings.endpoint.protocol().id().to_owned(),
            model: inner.settings.endpoint.model.clone(),
            key_present: inner.credential_key_present(),
            test: inner.endpoint_test.clone(),
        };
        let programs = inner.program_views();
        // An upgrade whose warnings changed shows them once more before the board;
        // a same-version open keeps the acknowledgement (F-06).
        let terms_acknowledged = inner.settings.terms_acknowledged
            && !(inner.upgrade.is_some() && inner.settings.terms_version < TERMS_VERSION);
        let setup_complete =
            inner.settings.endpoint.configured() && !inner.settings.agents.is_empty();
        let forgejo = ForgejoView {
            state: inner.forgejo.state.clone(),
            url: inner.forgejo.url.clone(),
            version: bundled_forgejo::VERSION.to_owned(),
            admin_login: inner.forgejo.admin_login.clone(),
            organisation: bundled_forgejo::ORGANISATION.to_owned(),
            repositories: inner.forgejo.repositories.len(),
            repository_names: {
                let mut names: Vec<String> = inner
                    .forgejo
                    .repositories
                    .iter()
                    .map(|repository| repository.full_name.clone())
                    .collect();
                names.sort();
                names
            },
            events: inner.forgejo.events.iter().cloned().collect(),
            admin_login_kept: inner.forgejo.admin_login_kept,
        };
        let agents = inner
            .agents
            .iter()
            .map(|agent| AgentView {
                name: agent.name.clone(),
                login: agent
                    .login
                    .clone()
                    .unwrap_or_else(|| format!("gewu-{}", agent.name)),
                provisioning: agent.provisioning.clone(),
                desired: agent.desired,
                session: agent.session.clone(),
                sessions_since_start: agent.sessions_since_start,
                runs_total: agent.runs_total,
                recent: agent.recent.iter().cloned().collect(),
                instruction_chars: agent.instruction_chars,
                instruction_first_line: agent.instruction_first_line.clone(),
                instruction: agent.instruction.clone(),
                instruction_lines: agent.instruction_lines,
                activity: agent.activity.clone(),
                activity_at: agent.activity_at,
                last_error: agent.last_error.clone(),
                last_result: agent.last_result.clone(),
                last_failure: agent
                    .leftover
                    .clone()
                    .or_else(|| agent.last_failure.clone()),
                leftover: agent.leftover.clone(),
                current_tool: agent.current_tool.clone(),
                current_tool_seconds: agent
                    .current_tool_since
                    .and_then(|since| SystemTime::now().duration_since(since).ok())
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0),
                retrying: agent.retrying.clone(),
                last_work: agent.last_work.clone(),
                no_assigned_work: agent.no_assigned_work,
                last_work_result: agent.last_work_result.clone(),
                last_result_is_work: agent.last_result_is_work,
                program: inner
                    .settings
                    .agent(&agent.name)
                    .map(|saved| saved.harness().id().to_owned())
                    .unwrap_or_else(|| "builtin".to_owned()),
                program_label: inner
                    .settings
                    .agent(&agent.name)
                    .map(|saved| {
                        saved
                            .unknown_harness()
                            .map(|unknown| format!("unknown program `{unknown}`"))
                            .unwrap_or_else(|| saved.harness().label().to_owned())
                    })
                    .unwrap_or_else(|| Program::Builtin.label().to_owned()),
                unknown_program: inner
                    .settings
                    .agent(&agent.name)
                    .and_then(|saved| saved.unknown_harness()),
                program_installed: inner
                    .settings
                    .agent(&agent.name)
                    .map(|saved| saved.harness())
                    .filter(|program| program.is_native())
                    .map(|program| harness::installed(&inner.options.data_root, program).is_some())
                    .unwrap_or(true),
                program_version: inner
                    .settings
                    .agent(&agent.name)
                    .and_then(|saved| harness::installed(&inner.options.data_root, saved.harness()))
                    .map(|installed| installed.version),
                protocol: inner
                    .settings
                    .agent(&agent.name)
                    .and_then(|saved| inner.connection_for(saved))
                    .map(|(protocol, _, _)| protocol.id().to_owned())
                    .unwrap_or_else(|| inner.settings.endpoint.protocol().id().to_owned()),
                base_url: inner
                    .settings
                    .agent(&agent.name)
                    .and_then(|saved| inner.connection_for(saved))
                    .map(|(_, base, _)| base)
                    .unwrap_or_else(|| inner.settings.endpoint.base_url()),
                model: inner
                    .settings
                    .agent(&agent.name)
                    .and_then(|saved| inner.connection_for(saved))
                    .map(|(_, _, model)| model)
                    .unwrap_or_else(|| inner.settings.endpoint.model.clone()),
                key_present: credentials::model_key(&inner.credentials, &agent.name).is_ok(),
                model_test: inner
                    .agent_tests
                    .get(&agent.name)
                    .cloned()
                    .unwrap_or(EndpointTest::NotRun),
                program_error: inner
                    .program_block(agent)
                    .or_else(|| agent.program_hint.clone()),
                active_program: inner.active_program(agent),
                next_program: inner.next_program(agent),
                codex_sandbox: inner
                    .settings
                    .agent(&agent.name)
                    .filter(|saved| saved.harness() == Program::Codex)
                    .map(|saved| {
                        if saved.codex_sandbox == "full-access" {
                            "full-access".to_owned()
                        } else {
                            "native".to_owned()
                        }
                    }),
            })
            .collect();
        BoardView {
            terms_acknowledged,
            terms: {
                let mut terms: Vec<String> = TERMS.iter().map(|term| (*term).to_owned()).collect();
                if !inner.options.confinement.confined() {
                    // The warning screen says what the board will also say, in one
                    // sentence, instead of pretending the run is confined.
                    terms.push("Runs are not confined on this computer.".to_owned());
                }
                terms
            },
            setup_complete,
            forgejo,
            agents,
            endpoint,
            programs,
            notice,
            admin_password,
            confined: inner.options.confinement.confined(),
            confinement: inner
                .options
                .confinement
                .confined()
                .then(|| inner.options.confinement.label().to_owned()),
            upgrade: inner.upgrade.clone(),
        }
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        if inner.stopping {
            return Err("the engine is shutting down".to_owned());
        }
        match command {
            Command::AcknowledgeTerms => {
                inner.commit_settings(|settings| {
                    settings.terms_acknowledged = true;
                    settings.terms_version = TERMS_VERSION;
                    Ok(())
                })?;
                inner.note("the terms are recorded");
                Ok(())
            }
            Command::SetEndpoint { url, model, key } => {
                let url = url.trim().to_owned();
                let model = model.trim().to_owned();
                let previous_key = credentials::model_key(&inner.credentials, "").ok();
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    credentials::record_secret(&inner.credentials, None, "model_key", key.trim())
                        .map_err(|error| format!("the model key could not be kept: {error}"))?;
                }
                if let Err(error) = inner.commit_settings(|settings| {
                    settings.endpoint.url = url;
                    settings.endpoint.model = model;
                    Ok(())
                }) {
                    return Err(restore_endpoint_key(&inner, previous_key, error));
                }
                inner.endpoint_test = EndpointTest::NotRun;
                inner.note("your model is saved");
                Ok(())
            }
            Command::SetConnection {
                base_url,
                protocol,
                model,
                key,
            } => {
                let base = base_url.trim().trim_end_matches('/').to_owned();
                let model = model.trim().to_owned();
                let Some(protocol) = Protocol::parse(&protocol) else {
                    return Err("that connection type is not one this product knows".to_owned());
                };
                if base.is_empty() || model.is_empty() {
                    return Err("the API address and model are both needed".to_owned());
                }
                if protocol != Protocol::ChatCompletions {
                    return Err(
                        "the built-in engine needs a Chat Completions connection".to_owned()
                    );
                }
                let previous_key = credentials::model_key(&inner.credentials, "").ok();
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    credentials::record_secret(&inner.credentials, None, "model_key", key.trim())
                        .map_err(|error| format!("the model key could not be kept: {error}"))?;
                }
                if let Err(error) = inner.commit_settings(|settings| {
                    settings.endpoint.url.clear();
                    settings.endpoint.base_url = base;
                    settings.endpoint.protocol = protocol.id().to_owned();
                    settings.endpoint.model = model;
                    Ok(())
                }) {
                    return Err(restore_endpoint_key(&inner, previous_key, error));
                }
                inner.endpoint_test = EndpointTest::NotRun;
                inner.note("the default connection is saved");
                Ok(())
            }
            Command::SetAgentProgram {
                name,
                program,
                protocol,
            } => {
                let Some(program) = Program::parse(&program) else {
                    return Err("that program is not one this product knows".to_owned());
                };
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let endpoint = inner.settings.endpoint.clone();
                let explicit_key = credentials::agent_model_key(&inner.credentials, &name)
                    .ok()
                    .flatten()
                    .is_some();
                let inherited_key = credentials::model_key(&inner.credentials, &name).ok();
                let settings_before = inner.settings.clone();
                let mut next = inner.settings.clone();
                let Some(saved) = next.agent_mut(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let requested = protocol.as_deref().and_then(Protocol::parse);
                if let Some(requested) = requested
                    && let Some(error) = harness::compatibility_error(program, requested)
                {
                    return Err(error);
                }
                if program.is_native() && saved.provider.is_none() && !endpoint.configured() {
                    return Err(
                        "set the default connection first, then choose this agent's program"
                            .to_owned(),
                    );
                }
                saved.set_harness(program);
                if program.is_native() {
                    if saved.provider.is_none() {
                        saved.provider = Some(settings::AgentProvider {
                            base_url: endpoint.base_url(),
                            protocol: endpoint.protocol().id().to_owned(),
                            model: endpoint.model.clone(),
                        });
                    }
                    let chosen = requested
                        .or_else(|| {
                            saved
                                .provider
                                .as_ref()
                                .and_then(|provider| Protocol::parse(&provider.protocol))
                        })
                        .filter(|protocol| program.supports(*protocol))
                        .unwrap_or_else(|| program.supported_protocols()[0]);
                    if let Some(provider) = saved.provider.as_mut() {
                        provider.protocol = chosen.id().to_owned();
                    }
                }
                if program != Program::Codex {
                    saved.codex_sandbox.clear();
                } else if saved.codex_sandbox.is_empty() {
                    saved.codex_sandbox = "native".to_owned();
                }
                if let Err(error) = next.save(&inner.settings_path) {
                    return Err(format!(
                        "the program change was not saved; the previous selection was restored ({error})"
                    ));
                }
                inner.settings = next;
                if program.is_native()
                    && !explicit_key
                    && let Some(key) = inherited_key.as_ref()
                {
                    if let Err(error) = credentials::record_secret(
                        &inner.credentials,
                        Some("model_keys"),
                        &name,
                        key.expose(),
                    ) {
                        inner.settings = settings_before;
                        let restore = inner.save_settings();
                        return Err(match restore {
                            Ok(()) => format!(
                                "the program change was rolled back: the current model key for `{name}` could not be saved ({error})"
                            ),
                            Err(restore_error) => format!(
                                "the program change could not be restored after a key-save failure ({error}); the settings file also failed to restore ({restore_error})"
                            ),
                        });
                    }
                }
                inner.agents[index].last_error = None;
                inner.agent_tests.remove(&name);
                let hint = if program == Program::Codex {
                    inner
                        .codex_sandbox_block(&inner.agents[index])
                        .map(|detail| codex_sandbox_action(&detail))
                } else {
                    None
                };
                inner.agents[index].program_hint = hint;
                if inner.agents[index].running.is_some() {
                    inner.note(format!(
                        "`{name}` now runs with {}; this run keeps its settings",
                        program.label()
                    ));
                } else {
                    inner.note(format!("`{name}` now runs with {}", program.label()));
                }
                Ok(())
            }
            Command::SetAgentModel {
                name,
                base_url,
                protocol,
                model,
                key,
            } => {
                let Some(protocol) = Protocol::parse(&protocol) else {
                    return Err("that connection type is not one this product knows".to_owned());
                };
                let base = base_url
                    .trim()
                    .trim_end_matches('/')
                    .trim_end_matches("/chat/completions")
                    .trim_end_matches("/responses")
                    .trim_end_matches("/messages")
                    .trim_end_matches('/')
                    .to_owned();
                let model = model.trim().to_owned();
                if base.is_empty() || model.is_empty() {
                    return Err("the API address and model are both needed".to_owned());
                }
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let current_program = inner.settings.agent(&name).map(|saved| saved.harness());
                if let Some(program) = current_program
                    && let Some(error) = harness::compatibility_error(program, protocol)
                {
                    return Err(error);
                }
                if key.as_deref().is_some_and(|key| key.trim().is_empty()) {
                    return Err("the API key is empty; type it, then press ⏎".to_owned());
                }
                if key.is_none() && credentials::model_key(&inner.credentials, &name).is_err() {
                    return Err("the API key is empty; type it, then press ⏎".to_owned());
                }
                let settings_before = inner.settings.clone();
                let mut next = inner.settings.clone();
                let Some(saved) = next.agent_mut(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                saved.provider = Some(settings::AgentProvider {
                    base_url: base,
                    protocol: protocol.id().to_owned(),
                    model,
                });
                if let Err(error) = next.save(&inner.settings_path) {
                    return Err(format!(
                        "the model change was not saved; the previous selection was restored ({error})"
                    ));
                }
                inner.settings = next;
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    if let Err(error) = credentials::record_secret(
                        &inner.credentials,
                        Some("model_keys"),
                        &name,
                        key.trim(),
                    ) {
                        inner.settings = settings_before;
                        let restore = inner.save_settings();
                        return Err(match restore {
                            Ok(()) => format!(
                                "the model change was rolled back: the key could not be saved ({error})"
                            ),
                            Err(restore_error) => format!(
                                "the model change could not be restored after a key-save failure ({error}); the settings file also failed to restore ({restore_error})"
                            ),
                        });
                    }
                }
                inner.agents[index].last_error = None;
                inner.agent_tests.remove(&name);
                // A model save is not a program or sandbox change: recompute the
                // sandbox prerequisite instead of hiding a cause that is still there.
                let hint = if inner
                    .settings
                    .agent(&name)
                    .is_some_and(|saved| saved.harness() == Program::Codex)
                {
                    inner
                        .codex_sandbox_block(&inner.agents[index])
                        .map(|detail| codex_sandbox_action(&detail))
                } else {
                    None
                };
                inner.agents[index].program_hint = hint;
                if inner.agents[index].running.is_some() {
                    inner.note(format!(
                        "`{name}`'s connection is saved for its next run; this run keeps its settings"
                    ));
                } else {
                    inner.note(format!("`{name}`'s connection is saved for its next run"));
                }
                Ok(())
            }
            Command::SetAgentCodexSandbox { name, mode } => {
                let mode = mode.trim();
                if mode != "native" && mode != "full-access" {
                    return Err(
                        "that Codex sandbox choice is not one this product knows".to_owned()
                    );
                }
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let mut next = inner.settings.clone();
                let Some(saved) = next.agent_mut(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                if saved.harness() != Program::Codex {
                    return Err("the sandbox choice applies only to Codex".to_owned());
                }
                saved.codex_sandbox = mode.to_owned();
                if let Err(error) = next.save(&inner.settings_path) {
                    return Err(format!(
                        "the sandbox choice was not saved; the previous selection was restored ({error})"
                    ));
                }
                inner.settings = next;
                inner.agents[index].last_error = None;
                let hint = if mode == "native" {
                    inner
                        .codex_sandbox_block(&inner.agents[index])
                        .map(|detail| codex_sandbox_action(&detail))
                } else {
                    None
                };
                inner.agents[index].program_hint = hint;
                if inner.agents[index].running.is_some() {
                    inner.note(format!(
                        "`{name}`'s Codex sandbox is saved for its next run; this run keeps its settings"
                    ));
                } else {
                    inner.note(format!(
                        "`{name}`'s Codex sandbox is saved for its next run"
                    ));
                }
                Ok(())
            }
            Command::InstallProgram { id } => {
                let Some(program) = Program::parse(&id).filter(|program| program.is_native())
                else {
                    return Err("there is no installable program with that name".to_owned());
                };
                if inner
                    .program_states
                    .get(&id)
                    .is_some_and(|state| state.installing.is_some())
                {
                    inner.note(format!("{} is already installing", program.label()));
                    return Ok(());
                }
                let cancel = harness::CancelToken::new();
                let state = inner.program_states.entry(id.clone()).or_default();
                state.installing = Some(None);
                state.phase = Some(harness::InstallPhase::Preparing);
                state.started = Some(Instant::now());
                state.error = None;
                state.cancel = Some(cancel.clone());
                inner.note(format!("installing {}", program.label()));
                let inner_arc = Arc::clone(&self.inner);
                let data_root = inner.options.data_root.clone();
                drop(inner);
                let handle = std::thread::spawn(move || {
                    let result =
                        harness::install_cancellable(&data_root, program, &cancel, |sample| {
                            if let Ok(mut inner) = inner_arc.lock()
                                && let Some(state) = inner.program_states.get_mut(program.id())
                            {
                                state.installing = Some(sample.percent);
                                state.phase = Some(sample.phase);
                            }
                        });
                    if let Ok(mut inner) = inner_arc.lock()
                        && let Some(state) = inner.program_states.get_mut(program.id())
                    {
                        state.installing = None;
                        state.phase = None;
                        state.started = None;
                        state.cancel = None;
                        match result {
                            Ok(installed) => {
                                state.error = None;
                                let names: Vec<String> = inner
                                    .settings
                                    .agents
                                    .iter()
                                    .filter(|saved| saved.harness() == program)
                                    .map(|saved| saved.name.clone())
                                    .collect();
                                for name in names {
                                    if let Some(index) = inner.agent_index(&name) {
                                        inner.agents[index].program_hint = None;
                                    }
                                }
                                inner.note(format!(
                                    "{} {} is installed",
                                    program.label(),
                                    installed.version
                                ));
                            }
                            Err(error) if error == harness::INSTALL_CANCELED => {
                                state.started = None;
                                state.error = None;
                                inner.note(format!(
                                    "installing {} was canceled; the previous version is unchanged",
                                    program.label()
                                ));
                            }
                            Err(error) => {
                                state.error = Some(crate::forgejo_tools::sentence(error.clone()));
                                inner.note(format!(
                                    "{} could not be installed: {error}",
                                    program.label()
                                ));
                            }
                        }
                    }
                });
                if let Some(state) = lock_inner(&self.inner).program_states.get_mut(&id) {
                    state.thread = Some(handle);
                }
                Ok(())
            }
            Command::CancelInstallProgram { id } => {
                let Some(program) = Program::parse(&id).filter(|program| program.is_native())
                else {
                    return Err("there is no installable program with that name".to_owned());
                };
                let Some(state) = inner.program_states.get(&id) else {
                    inner.note(format!("{} is not installing", program.label()));
                    return Ok(());
                };
                if state.installing.is_none() {
                    inner.note(format!("{} is not installing", program.label()));
                    return Ok(());
                }
                if let Some(cancel) = &state.cancel {
                    cancel.cancel();
                }
                inner.note(format!(
                    "canceling the {} install; the previous version stays usable",
                    program.label()
                ));
                Ok(())
            }
            Command::TestAgentModel { name } => {
                let Some(saved) = inner.settings.agent(&name).cloned() else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let protocol = saved
                    .provider()
                    .map(|provider| provider.protocol())
                    .unwrap_or_else(|| inner.settings.endpoint.protocol());
                let base_url = saved
                    .provider()
                    .map(|provider| provider.base_url.clone())
                    .filter(|base| !base.trim().is_empty())
                    .unwrap_or_else(|| inner.settings.endpoint.base_url());
                let model = saved
                    .provider()
                    .map(|provider| provider.model.clone())
                    .filter(|model| !model.trim().is_empty())
                    .unwrap_or_else(|| inner.settings.endpoint.model.clone());
                if base_url.trim().is_empty() || model.trim().is_empty() {
                    return Err("the API address and model are both needed".to_owned());
                }
                let key = match credentials::model_key(&inner.credentials, &name) {
                    Ok(key) => key,
                    Err(_) => return Err("the API key is empty; type it, then press ⏎".to_owned()),
                };
                inner
                    .agent_tests
                    .insert(name.clone(), EndpointTest::Running);
                let inner_arc = Arc::clone(&self.inner);
                drop(inner);
                std::thread::spawn(move || {
                    let result = crate::model::probe_protocol(
                        &base_url,
                        protocol,
                        &model,
                        &key,
                        crate::model::PROBE_TIMEOUT,
                    )
                    .map_err(|error| plain_endpoint_failure(&error, &model));
                    if let Ok(mut inner) = inner_arc.lock() {
                        inner.agent_tests.insert(
                            name,
                            match result {
                                Ok(_) => EndpointTest::Passed {
                                    answer: "the model answered".to_owned(),
                                },
                                Err(reason) => EndpointTest::Failed { reason },
                            },
                        );
                    }
                });
                Ok(())
            }
            Command::TestEndpoint => {
                inner.endpoint_test = EndpointTest::Running;
                let inner_arc = Arc::clone(&self.inner);
                let endpoint = inner.settings.endpoint.url.clone();
                let model = inner.settings.endpoint.model.clone();
                let credentials = inner.credentials.clone();
                drop(inner);
                std::thread::spawn(move || {
                    endpoint_test_job(inner_arc, endpoint, model, credentials);
                });
                Ok(())
            }
            Command::CreateAgents { count } => {
                if count == 0 {
                    return Err("the count must be at least 1".to_owned());
                }
                if count > MAX_AGENTS {
                    return Err(format!("at most {MAX_AGENTS} agents can be made"));
                }
                let settings_before = inner.settings.clone();
                let mut next = inner.settings.clone();
                let mut keys: Vec<(String, String)> = Vec::new();
                let mut names: Vec<String> = Vec::new();
                for number in 1..=count {
                    let name = format!("agent-{number}");
                    if next.agent(&name).is_none() {
                        next.add_agent(&name, Desired::Stopped)?;
                        if let Some(key) = inner.assign_new_agent_program(&mut next, &name)? {
                            keys.push((name.clone(), key));
                        }
                        ensure_agent_paths(&inner.options.layout, &name)?;
                        names.push(name);
                    }
                }
                next.save(&inner.settings_path)?;
                inner.settings = next;
                for (name, key) in keys {
                    if let Err(error) = credentials::record_secret(
                        &inner.credentials,
                        Some("model_keys"),
                        &name,
                        &key,
                    ) {
                        inner.settings = settings_before;
                        let _ = inner.save_settings();
                        return Err(format!(
                            "{name} was created, but its current model key could not be saved: {error}. Its next run will show the missing-key action; no global key is substituted."
                        ));
                    }
                }
                for name in names {
                    let runtime = AgentRuntime::load(&inner, &name);
                    inner.agents.push(runtime);
                }
                // A new workspace is a new bound tree; re-run the layout decision so
                // a workspace that cannot be confined takes the claim off the board
                // now (B9-R-R).
                inner.refresh_confinement();
                let count = inner.agents.len();
                inner.note(format!("{count} agents are ready"));
                Ok(())
            }
            Command::AddAgent => {
                let settings_before = inner.settings.clone();
                let mut next = inner.settings.clone();
                let name = next.allocate_agent_name()?;
                next.add_agent(&name, Desired::Stopped)?;
                let key = inner.assign_new_agent_program(&mut next, &name)?;
                ensure_agent_paths(&inner.options.layout, &name)?;
                next.save(&inner.settings_path)?;
                inner.settings = next;
                if let Some(key) = key
                    && let Err(error) = credentials::record_secret(
                        &inner.credentials,
                        Some("model_keys"),
                        &name,
                        &key,
                    )
                {
                    inner.settings = settings_before;
                    let _ = inner.save_settings();
                    return Err(format!(
                        "{name} was created, but its current model key could not be saved: {error}. Its next run will show the missing-key action; no global key is substituted."
                    ));
                }
                let runtime = AgentRuntime::load(&inner, &name);
                inner.agents.push(runtime);
                inner.refresh_confinement();
                inner.note(format!("`{name}` was added"));
                Ok(())
            }
            Command::RemoveAgent { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                inner.agent_tests.remove(&name);
                let grace = inner.options.stop_grace;
                stop_agent(&mut inner.agents[index], grace)?;
                // The stop already happened; the runtime must say Stopped even if
                // the settings file cannot record it, or the scheduler restarts.
                inner.agents[index].desired = Desired::Stopped;
                let committed = inner.commit_settings(|settings| {
                    if let Some(saved) = settings.agent_mut(&name) {
                        saved.set_desired(Desired::Stopped);
                    }
                    Ok(())
                });
                if let Err(error) = committed {
                    inner.agents[index].removal_pending = false;
                    inner.agents[index].removal_retry_at = None;
                    return Err(format!(
                        "`{name}` was stopped, but the removal could not be saved and the stopped state was not saved; it will start again the next time the program opens ({error})"
                    ));
                }
                inner.agents[index].removal_pending = true;
                inner.agents[index].removal_retry_at = None;
                inner.note(format!("`{name}` is being removed"));
                Ok(())
            }
            Command::Start { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                if removal_marked(&inner.options.layout, &name) {
                    let reason = "its Forgejo sign-in and tokens were already cancelled; Remove can be retried";
                    inner.agents[index].session = SessionState::Blocked {
                        reason: reason.to_owned(),
                    };
                    inner.note(format!("not started — {reason}"));
                    return Ok(());
                }
                // A cleanup that failed at open or at a run boundary is retried
                // before any replacement run. Until it succeeds, Start reports the
                // reason and does not start; once the old group is gone, the
                // records are cleared and the run can start normally.
                let leftover_targets = inner.agents[index].leftover_targets.clone();
                if !leftover_targets.is_empty() {
                    let mut failed = None;
                    for target in &leftover_targets {
                        let result = match target {
                            RecoveryTarget::Run(run_dir) => {
                                run::recover_ending(run_dir, inner.options.stop_grace).map(|_| ())
                            }
                            RecoveryTarget::Agent(agent_dir) => {
                                recover_agent_directory(agent_dir, inner.options.stop_grace)
                            }
                        };
                        if let Err(reason) = result {
                            failed = Some(reason);
                            break;
                        }
                    }
                    if let Some(reason) = failed {
                        inner.agents[index].leftover = Some(reason.clone());
                        let sentence = format!("cleanup unfinished: {reason}");
                        inner.note(sentence.clone());
                        return Err(sentence);
                    }
                    inner.agents[index].leftover = None;
                    inner.agents[index].leftover_targets.clear();
                    inner.agents[index].session = SessionState::Idle;
                    inner.agents[index].next_at = None;
                }
                let sandbox_block = inner.codex_sandbox_block(&inner.agents[index]);
                if let Some(reason) = inner
                    .program_block(&inner.agents[index])
                    .or_else(|| sandbox_block.map(|detail| codex_sandbox_action(&detail)))
                {
                    inner.agents[index].session = SessionState::Blocked {
                        reason: reason.clone(),
                    };
                    inner.agents[index].program_hint = Some(reason.clone());
                    inner.note(format!("not started — {reason}"));
                    return Ok(());
                }
                inner.agents[index].program_hint = None;
                inner.set_desired(index, Desired::Started)?;
                let agent = &mut inner.agents[index];
                agent.sessions_since_start = 0;
                agent.session_number = 0;
                agent.quick_failures = 0;
                agent.next_at = None;
                let no_instruction = agent.instruction_chars == 0;
                if no_instruction {
                    agent.session = SessionState::Blocked {
                        reason: "needs an instruction".to_owned(),
                    };
                }
                inner.note(if no_instruction {
                    "not started — write an instruction first".to_owned()
                } else {
                    format!("`{name}` started")
                });
                Ok(())
            }
            Command::Stop { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                let grace = inner.options.stop_grace;
                stop_agent(&mut inner.agents[index], grace)?;
                inner.agents[index].desired = Desired::Stopped;
                match inner.commit_settings(|settings| {
                    if let Some(saved) = settings.agent_mut(&name) {
                        saved.set_desired(Desired::Stopped);
                    }
                    Ok(())
                }) {
                    Ok(()) => {
                        inner.note(format!("`{name}` stopped"));
                        Ok(())
                    }
                    Err(error) => Err(format!(
                        "`{name}` was stopped, but the stopped state could not be saved; it will start again the next time the program opens ({error})"
                    )),
                }
            }
            Command::StartForgejo => {
                if matches!(
                    inner.forgejo.state,
                    ForgejoState::Running
                        | ForgejoState::Starting
                        | ForgejoState::Preparing
                        | ForgejoState::Fetching { .. }
                ) || inner.forgejo.start_inflight
                {
                    inner.note("Forgejo is already starting");
                    return Ok(());
                }
                inner.forgejo.start_requested = true;
                inner.forgejo.retry_start = false;
                inner.forgejo.retry_at = None;
                inner.forgejo.state = ForgejoState::Preparing;
                inner.note("Forgejo is starting");
                Ok(())
            }
            Command::RevealAdminPassword => {
                if !matches!(inner.forgejo.state, ForgejoState::Running) {
                    return Err(
                        "Forgejo is not running, so its password cannot be reset".to_owned()
                    );
                }
                if inner.password_reset_inflight {
                    inner.note("the password is already being reset");
                    return Ok(());
                }
                inner.password_reset_inflight = true;
                let inner_arc = Arc::clone(&self.inner);
                let url = inner.forgejo.url.clone();
                let login = inner.forgejo.admin_login.clone();
                let token = inner.forgejo.admin_token.clone();
                let Some(token) = token else {
                    return Err("the Forgejo administrator token is not ready".to_owned());
                };
                drop(inner);
                std::thread::spawn(move || {
                    let result = ForgejoClient::new(&url, token, DEFAULT_TIMEOUT)
                        .map_err(|error| sentence(&error))
                        .and_then(|client| bundled_forgejo::reset_admin_password(&client, &login));
                    if let Ok(mut inner) = inner_arc.lock() {
                        inner.password_reset_inflight = false;
                        if inner.stopping {
                            return;
                        }
                        match result {
                            Ok(password) => {
                                inner.admin_password_once = Some(password);
                            }
                            Err(reason) => inner.note(reason),
                        }
                    }
                });
                Ok(())
            }
        }
    }

    fn tick(&mut self) {
        self.tick_at();
    }

    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.options
            .layout
            .agent(agent)
            .ok()
            .map(|paths| paths.instruction().to_path_buf())
    }

    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        let inner = self.inner.lock().ok()?;
        let index = inner.agent_index(agent)?;
        let runtime = &inner.agents[index];
        let dir = runtime
            .running
            .as_ref()
            .map(|run| run.run_dir.clone())
            .or_else(|| runtime.newest_run.clone())?;
        Some((dir.join(LOG_FILE), dir.join(TRANSCRIPT_FILE)))
    }

    fn readable_transcript(&self, agent: &str) -> Option<PathBuf> {
        let inner = self.inner.lock().ok()?;
        let index = inner.agent_index(agent)?;
        let runtime = &inner.agents[index];
        let dir = runtime
            .running
            .as_ref()
            .map(|run| run.run_dir.clone())
            .or_else(|| runtime.newest_run.clone())?;
        run::write_readable_transcript(&dir).ok()
    }

    fn shutdown(&mut self, grace: Duration) {
        let started = Instant::now();
        let mut first_ending_errors: Vec<String> = Vec::new();
        {
            let mut inner = lock_inner(&self.inner);
            if inner.stopping {
                return;
            }
            inner.stopping = true;
            inner.generation = inner.generation.saturating_add(1);
            for state in inner.program_states.values() {
                if let Some(cancel) = &state.cancel {
                    cancel.cancel();
                }
            }
            // Ask every group politely. A start or spawn still in flight records its
            // child's pid, and is signalled again as that pid appears below.
            for agent in &mut inner.agents {
                if let Some(run) = agent.running.as_mut() {
                    let proposed = proposed_first_ending(run);
                    let requested = run.run.request_stop(proposed);
                    match requested {
                        Ok(ending) => {
                            run.first_ending = Some(ending);
                            run.stopping = true;
                        }
                        Err(error) => first_ending_errors.push(format!(
                            "`{}`: the shutdown ending is uncertain: {error}",
                            agent.name
                        )),
                    }
                }
                if let Some(pid) = agent.starting_pid {
                    run::request_starting_stop(pid, false, "the starting run was stopped");
                }
            }
            if let Some(server) = inner.forgejo.child.as_mut() {
                bundled_forgejo::signal_process_group(server.pid(), false);
            }
            if let Some(pid) = inner.forgejo.start_pid {
                bundled_forgejo::signal_process_group(pid, false);
            }
            for pid in &inner.forgejo.helper_pids {
                bundled_forgejo::signal_process_group(*pid, false);
            }
        }
        if !first_ending_errors.is_empty() {
            let mut inner = lock_inner(&self.inner);
            for note in first_ending_errors.drain(..) {
                inner.note(note);
            }
            // A failed first attempt is retried before the forced stop below.
        }
        // The bounded hand-over: a start or spawn that is between spawning its child
        // and returning must see the new generation and stop that child itself. The
        // engine lock is not held here, so that work can finish. Pids that appear
        // during the wait are signalled as soon as they are visible.
        let mut signalled: Vec<u32> = Vec::new();
        // Runs whose supervisor has ended are settled after the wait: their group is
        // stopped, a durable ending is written (unless the run wrote one) and only
        // then are the identity handles removed. A run that cannot be settled keeps
        // its handles, so the next open reports the failure and Start retries.
        let mut settled_runs: Vec<(PathBuf, u32, Option<run::FirstEnding>)> = Vec::new();
        loop {
            let mut appeared: Vec<u32> = Vec::new();
            let mut run_appeared: Vec<u32> = Vec::new();
            let mut alive = false;
            {
                let mut inner = lock_inner(&self.inner);
                for agent in &mut inner.agents {
                    if let Some(run) = agent.running.as_mut() {
                        if run.run.try_status().is_some() {
                            settled_runs.push((
                                run.run_dir.clone(),
                                run.run.pid(),
                                run.first_ending,
                            ));
                            if let Some(run_id) = run_id_in(&run.run_dir) {
                                crate::confinement::remove_run_temp(run.run.pid(), run_id);
                            }
                            agent.running = None;
                        } else {
                            alive = true;
                        }
                    }
                    if agent.starting {
                        alive = true;
                        if let Some(pid) = agent.starting_pid {
                            run_appeared.push(pid);
                        }
                    }
                }
                if inner.forgejo.start_inflight {
                    alive = true;
                    if let Some(pid) = inner.forgejo.start_pid {
                        appeared.push(pid);
                    }
                }
                if inner.forgejo.helper_inflight > 0 {
                    alive = true;
                }
                for pid in &inner.forgejo.helper_pids {
                    appeared.push(*pid);
                }
                if let Some(server) = inner.forgejo.child.as_ref() {
                    alive = true;
                    appeared.push(server.pid());
                }
            }
            for pid in run_appeared {
                if !signalled.contains(&pid) {
                    signalled.push(pid);
                    run::request_starting_stop(pid, false, "the starting run was stopped");
                }
            }
            for pid in appeared {
                if !signalled.contains(&pid) {
                    signalled.push(pid);
                    bundled_forgejo::signal_process_group(pid, false);
                }
            }
            if !alive || started.elapsed() >= grace {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Force whatever is left, then reap it. The lock is held for this last short
        // phase; the jobs that own an in-flight child see the generation changed and
        // stop and reap it on their own thread.
        {
            let mut inner = lock_inner(&self.inner);
            for agent in &mut inner.agents {
                if let Some(mut run) = agent.running.take() {
                    let run_id = run_id_in(&run.run_dir).map(str::to_owned);
                    let mut uncertain = false;
                    if run.first_ending.is_none() {
                        let proposed = proposed_first_ending(&run);
                        match run_id
                            .as_deref()
                            .ok_or_else(|| "the run directory has no run id".to_owned())
                            .and_then(|run_id| run::prepare_ending(&run.run_dir, run_id, proposed))
                        {
                            Ok(ending) => run.first_ending = Some(ending),
                            Err(error) => {
                                let reason =
                                    run.run.stop_uncertain(&error, Duration::from_millis(500));
                                first_ending_errors.push(format!("`{}`: {reason}", agent.name));
                                uncertain = true;
                            }
                        }
                    }
                    if !uncertain {
                        let proposed = run.first_ending.unwrap_or(run::FirstEnding::Stop);
                        let result = run.run.request_stop_and_wait(
                            proposed,
                            "the run was stopped when the program closed",
                            Duration::from_millis(500),
                        );
                        match result {
                            Ok(()) => {
                                let first_ending =
                                    run.first_ending.unwrap_or(run::FirstEnding::Stop);
                                settled_runs.push((
                                    run.run_dir.clone(),
                                    run.run.pid(),
                                    Some(first_ending),
                                ));
                            }
                            Err(error) => {
                                let reason =
                                    run.run.stop_uncertain(&error, Duration::from_millis(500));
                                first_ending_errors.push(format!("`{}`: {reason}", agent.name));
                            }
                        }
                    }
                }
                if let Some(pid) = agent.starting_pid.take() {
                    run::request_starting_stop(pid, true, "the starting run was forced");
                }
                agent.starting = false;
                agent.session = SessionState::Idle;
                agent.next_at = None;
            }
            if let Some(mut server) = inner.forgejo.child.take() {
                server.stop(Duration::from_millis(100));
                debug_assert!(!process_alive(server.pid()));
            }
            if let Some(pid) = inner.forgejo.start_pid.take() {
                // The job that spawned this child owns its reaping. It has seen the
                // generation change; this is the last bound if it is still blocked.
                bundled_forgejo::signal_process_group(pid, true);
            }
            for pid in std::mem::take(&mut inner.forgejo.helper_pids) {
                // The token helper's job owns the wait and reaping; the force below
                // ends it if it is still in the grace window's shadow.
                bundled_forgejo::signal_process_group(pid, true);
            }
            inner.forgejo.state = ForgejoState::Stopped;
            inner.forgejo.admin_token = None;
            inner.forgejo.start_requested = false;
            inner.forgejo.start_inflight = false;
            inner.forgejo.provisioned = false;
        }
        if !first_ending_errors.is_empty() {
            let mut inner = lock_inner(&self.inner);
            for note in first_ending_errors.drain(..) {
                inner.note(note);
            }
        }
        // Settle every run whose supervisor this shutdown ended, outside the lock.
        // The run gets a durable ending before its handles go; if either step fails
        // the handles stay for the next open, which reports and retries them.
        for (run_dir, pid, first_ending) in settled_runs {
            let _ = run::request_finish(
                &run_dir,
                pid,
                run::RunOutcome::Stopped,
                Some("the run was stopped when the program closed".to_owned()),
                Duration::from_millis(500),
                Some("the run was stopped when the program closed".to_owned()),
                first_ending.unwrap_or(run::FirstEnding::Stop),
            );
        }
        // A token helper may have been between "recorded as in flight" and "spawn"
        // when the force phase ran. The job that started it checks the stop flag
        // before spawning and stops and reaps a child it did start; wait briefly for
        // that hand-over, so no helper survives the call.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let pending = lock_inner(&self.inner).forgejo.helper_inflight;
            if pending == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Cancelled installs clean their own stage. Wait inside the caller's grace so
        // a quit does not leave a download writing behind the board; a stage that
        // cannot finish is journaled and recovered on the next open.
        let install_jobs: Vec<std::thread::JoinHandle<()>> = {
            let mut inner = lock_inner(&self.inner);
            for state in inner.program_states.values_mut() {
                if let Some(cancel) = &state.cancel {
                    cancel.cancel();
                }
            }
            inner
                .program_states
                .values_mut()
                .filter_map(|state| state.thread.take())
                .collect()
        };
        let install_deadline = Instant::now() + grace.min(Duration::from_secs(2));
        for handle in install_jobs {
            while !handle.is_finished() && Instant::now() < install_deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        // A clean exit records the version too, so an installation that is opened and
        // closed many times keeps one current line.
        let root = lock_inner(&self.inner).options.layout.root().to_path_buf();
        let _ = last_seen::write(&root, env!("CARGO_PKG_VERSION"));
    }
}

/// Take the engine state lock, recovering from a poisoned holder.
fn lock_inner(inner: &Arc<Mutex<Inner>>) -> std::sync::MutexGuard<'_, Inner> {
    match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let grace = self.options.stop_grace;
        self.shutdown(grace);
    }
}

/// Remove a run's private temporary tree when its process is gone.
///
/// A run removes its own tree on every exit it can see. A `SIGKILL`, a crash after
/// the run's pid record was reaped, or the program itself being killed can leave
/// the tree behind; the pid and the run id are in its name, so a dead owner is
/// recognised exactly and nothing of a live run is touched (F-01).
fn remove_stale_run_temps() {
    let Some(temp) = crate::confinement::resolved_temp() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&temp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(crate::confinement::RUN_TEMP_PREFIX) else {
            continue;
        };
        let Some((pid, run_id)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let live_owner = bundled_forgejo::command_line(pid).is_some_and(|parts| {
            parts.iter().any(|part| part == "--run-agent")
                && parts.iter().any(|part| part.ends_with(run_id))
        });
        if !live_owner {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The run id a run directory's name carries, for the private-temp name.
fn run_id_in(run_dir: &Path) -> Option<&str> {
    run_dir.file_name().and_then(std::ffi::OsStr::to_str)
}

/// Re-enumerate one agent's run directories for a retried cleanup. An unreadable
/// agent directory remains a failure; a repaired one is rescanned instead of being
/// handed to the single-run reaper (review-3b).
fn recover_agent_directory(agent_dir: &Path, grace: Duration) -> Result<(), String> {
    let runs = fs::read_dir(agent_dir).map_err(|error| {
        format!(
            "the agent's run directory {} cannot be read: {error}",
            agent_dir.display()
        )
    })?;
    let mut reasons = Vec::new();
    for run in runs.flatten() {
        let run_dir = run.path();
        match fs::metadata(&run_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                reasons.push(format!(
                    "a run directory under {} cannot be inspected: {error}",
                    agent_dir.display()
                ));
                continue;
            }
        }
        if let Err(reason) = run::recover_ending(&run_dir, grace) {
            reasons.push(reason);
        }
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons.join("; "))
    }
}

/// Stop every run process group an earlier program left behind.
///
/// The state root is walked without following the agents' own settings: a run
/// directory is two levels down. A `session.pid` whose supervisor is still alive,
/// a supervisor killed by `SIGKILL`, and a shell whose supervisor removed its own
/// records are all found from the run directory: the supervisor's `session.*`
/// records or the shell record written at spawn. The group is never believed from
/// a bare number; the process environment is never read. The `run_stopped` record
/// is written before the caller opens (F-02/F-03).
#[allow(clippy::type_complexity)]
fn recover_stale_sessions(
    layout: &Layout,
    grace: Duration,
) -> Result<(Vec<u32>, Vec<(String, RecoveryTarget, String)>), String> {
    let mut stopped = Vec::new();
    let mut leftovers = Vec::new();
    let agents = match fs::read_dir(layout.state_root()) {
        Ok(agents) => agents,
        // A fresh installation has no state root yet: that is "no runs", not a
        // recovery failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((stopped, leftovers));
        }
        Err(error) => {
            return Err(format!(
                "the state root cannot be read, so an earlier run cannot be checked: {error}"
            ));
        }
    };
    for agent in agents.flatten() {
        let agent_path = agent.path();
        if !agent_path.is_dir() {
            continue;
        }
        let agent_name = agent.file_name().to_str().unwrap_or("an agent").to_owned();
        // read permission is not search permission; a child-name stat makes that
        // distinction visible instead of looking like an empty agent.
        match fs::metadata(agent_path.join(".gewu-search-probe")) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                leftovers.push((
                    agent_name.clone(),
                    RecoveryTarget::Agent(agent_path.clone()),
                    format!("the agent's run directory cannot be searched: {error}"),
                ));
                continue;
            }
        }
        let runs = match fs::read_dir(&agent_path) {
            Ok(runs) => runs,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                leftovers.push((
                    agent_name.clone(),
                    RecoveryTarget::Agent(agent_path.clone()),
                    format!("the agent's run directory cannot be read: {error}"),
                ));
                continue;
            }
        };
        for run in runs.flatten() {
            let run_dir = run.path();
            match fs::metadata(&run_dir) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    leftovers.push((
                        agent_name.clone(),
                        RecoveryTarget::Agent(agent_path.clone()),
                        format!(
                            "a run directory under {} cannot be inspected: {error}",
                            agent_path.display()
                        ),
                    ));
                    continue;
                }
            }
            // `recorded_group` is only the supervisor pid for the private run-temp
            // path; recovery itself runs even when the records are unreadable, so
            // the failure can reach the card.
            let recorded = run::recorded_group(&run_dir);
            match run::recover_ending(&run_dir, grace) {
                Ok(true) => {
                    if let Some((pid, _)) = recorded {
                        stopped.push(pid);
                    }
                }
                Ok(false) => {
                    // Nothing was alive: the records are stale, not evidence of a
                    // process this program must keep watching. They were removed
                    // with the run's shell record already.
                }
                Err(reason) => {
                    leftovers.push((
                        agent_name.clone(),
                        RecoveryTarget::Run(run_dir.clone()),
                        reason,
                    ));
                }
            }
            if let Some((pid, _)) = recorded
                && let Some(run_id) = run_id_in(&run_dir)
            {
                crate::confinement::remove_run_temp(pid, run_id);
            }
        }
    }
    Ok((stopped, leftovers))
}

fn ensure_agent_paths(layout: &Layout, name: &str) -> Result<(), String> {
    layout
        .ensure_agent(name)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// What the run's own record suggests before the owner pins the fact. This is only
/// an input to the owner; it never writes or signals.
fn proposed_first_ending(run: &RunningState) -> run::FirstEnding {
    run.first_ending.unwrap_or_else(|| {
        if run::terminal_recorded(&run.run_dir).unwrap_or(true) {
            run::FirstEnding::Terminal
        } else {
            run::FirstEnding::Stop
        }
    })
}

fn stop_agent(agent: &mut AgentRuntime, grace: Duration) -> Result<(), String> {
    if agent.starting {
        // The job checks the desired choice after every wait, stops a child it has
        // already spawned, and clears `starting` itself.
        if let Some(pid) = agent.starting_pid {
            run::request_starting_stop(pid, false, "the starting run was stopped");
        }
        agent.starting = false;
    }
    if let Some(run) = agent.running.as_mut() {
        // The owner records the fact, then sends the polite signal. A fact failure
        // marks the ending uncertain and sends no signal.
        let proposed = proposed_first_ending(run);
        let ending = run.run.request_stop(proposed)?;
        run.first_ending = Some(ending);
        run.stopping = true;
        run.stop_requested = true;
        run.stop_deadline = Some(Instant::now() + grace);
    }
    agent.next_at = None;
    agent.session = SessionState::Idle;
    Ok(())
}

// ---- background work -------------------------------------------------------

fn forgejo_start_job(inner: Arc<Mutex<Inner>>, options: EngineOptions, generation: u64) {
    forgejo_start_job_inner(Arc::clone(&inner), options, generation);
    // The job is over however it ended: a shutdown waiting for the hand-over must
    // stop waiting, and no pid may stay in the in-flight slot.
    let mut guard = lock_inner(&inner);
    guard.forgejo.start_inflight = false;
    guard.forgejo.start_pid = None;
}

fn forgejo_start_job_inner(inner: Arc<Mutex<Inner>>, options: EngineOptions, generation: u64) {
    let binary = bundled_forgejo::binary_path(&options.data_root);
    let data_root = options.data_root.clone();
    let ready_timeout = options.ready_timeout;
    let (admin_login, retry) = match inner.lock() {
        Ok(inner) => (inner.forgejo.admin_login.clone(), inner.forgejo.retry_start),
        Err(poisoned) => {
            let inner = poisoned.into_inner();
            (inner.forgejo.admin_login.clone(), inner.forgejo.retry_start)
        }
    };
    // A test starter supplies its own child; nothing is installed or prepared for
    // the real binary in that case.
    #[cfg(test)]
    let test_starter = options.forgejo_starter.is_some();
    #[cfg(not(test))]
    let test_starter = false;
    let had_binary = test_starter
        || (binary.is_file()
            && bundled_forgejo::sha256_file(&binary)
                .is_ok_and(|hash| hash == bundled_forgejo::SHA256));
    let installed = if had_binary {
        Ok(binary.clone())
    } else {
        let progress_inner = Arc::clone(&inner);
        bundled_forgejo::install_forgejo_at(&data_root, move |progress| {
            if let Ok(mut inner) = progress_inner.lock() {
                if inner.generation == generation && !inner.stopping {
                    inner.forgejo.state = ForgejoState::Fetching {
                        percent: progress.percent,
                    };
                }
            }
        })
    };
    let install_succeeded = installed.is_ok();
    let result = installed.and_then(|binary| {
        if let Ok(mut inner) = inner.lock() {
            if inner.generation == generation && !inner.stopping {
                inner.forgejo.state = ForgejoState::Preparing;
            }
        }
        let cancel_inner = Arc::clone(&inner);
        let cancel = move || {
            let inner = lock_inner(&cancel_inner);
            inner.stopping || inner.generation != generation
        };
        let prepared = if test_starter {
            bundled_forgejo::PreparedInstance {
                paths: bundled_forgejo::InstancePaths::under(&data_root),
                admin_login: admin_login.clone(),
                port: 3000,
                url: "http://127.0.0.1:3000".to_owned(),
                admin_token: None,
            }
        } else {
            bundled_forgejo::prepare_instance_checked(&data_root, &binary, &admin_login, &cancel)?
        };
        // A start that created the account also minted its administrator token before
        // the server came up. Record it now, so no later step needs the password.
        if let Some(token) = prepared.admin_token.as_ref() {
            let credentials_path = match inner.lock() {
                Ok(inner) => Some(inner.credentials.clone()),
                Err(poisoned) => Some(poisoned.into_inner().credentials.clone()),
            };
            if let Some(path) = credentials_path {
                let _ =
                    credentials::record_secret(&path, None, "forgejo_admin_token", token.expose());
                if let Ok(mut inner) = inner.lock() {
                    inner.forgejo.admin_token = Some(token.clone());
                }
            }
        }
        if let Ok(mut inner) = inner.lock() {
            if inner.generation == generation && !inner.stopping {
                inner.forgejo.admin_login = prepared.admin_login.clone();
                inner.forgejo.url = prepared.url.clone();
                inner.settings.forgejo.admin_login = prepared.admin_login.clone();
                let _ = inner.save_settings();
                inner.forgejo.state = ForgejoState::Starting;
            }
        }
        let record_inner = Arc::clone(&inner);
        let mut record = move |pid| {
            // Record the child before any readiness wait, even if shutdown has
            // already begun: its bounded hand-over looks for this pid.
            let mut inner = lock_inner(&record_inner);
            inner.forgejo.start_pid = Some(pid);
        };
        let server = start_forgejo(
            &options,
            &binary,
            prepared.clone(),
            ready_timeout,
            &mut record,
            &cancel,
        )?;
        Ok((server, prepared))
    });
    match result {
        Ok((server, prepared)) => {
            let url = prepared.url.clone();
            // Store the child as soon as it is up, before the token and provisioning
            // work: `shutdown` must be able to stop a server that is still being
            // provisioned. Only the short store happens under the lock.
            {
                let mut inner = lock_inner(&inner);
                if inner.generation != generation || inner.stopping {
                    drop(inner);
                    let mut server = server;
                    server.stop(Duration::from_secs(2));
                    return;
                }
                inner.forgejo.child = Some(server);
                inner.forgejo.start_pid = None;
                inner.forgejo.state = ForgejoState::Running;
                inner.forgejo.url = url.clone();
                inner.forgejo.admin_login = prepared.admin_login.clone();
                inner.forgejo.start_inflight = false;
                inner.forgejo.failures = 0;
                inner.forgejo.retry_start = false;
                inner.forgejo.retry_at = None;
                inner.forgejo.provision_inflight = false;
                inner.forgejo.provision_retry_at = None;
                inner.note(format!("Forgejo is ready at {url}"));
            }
            let token_cancel = {
                let inner = Arc::clone(&inner);
                move || {
                    let inner = lock_inner(&inner);
                    inner.stopping || inner.generation != generation
                }
            };
            let token = ensure_admin_token(
                &inner,
                &binary,
                &data_root,
                &prepared.admin_login,
                &token_cancel,
            );
            let provisioned = token.as_ref().ok().map(|token| {
                let client = ForgejoClient::new(&url, token.clone(), DEFAULT_TIMEOUT);
                client
                    .ok()
                    .and_then(|client| bundled_forgejo::provision(&client).ok())
            });
            if let Ok(mut inner) = inner.lock() {
                if inner.generation != generation || inner.stopping {
                    return;
                }
                inner.forgejo.admin_token = token.ok();
                match provisioned {
                    Some(Some(repositories)) => {
                        inner.forgejo.repositories = repositories;
                        inner.forgejo.provisioned = true;
                        inner.forgejo.repositories_refreshed_at = Some(Instant::now());
                    }
                    _ => {
                        inner.forgejo.provisioned = false;
                        inner.forgejo.provision_retry_at = Some(Instant::now() + PROVISION_RETRY);
                    }
                }
            }
        }
        Err(_reason) => {
            let was_fetching = !had_binary && !install_succeeded;
            let mut inner = lock_inner(&inner);
            if inner.generation != generation || inner.stopping {
                return;
            }
            inner.forgejo.start_inflight = false;
            inner.forgejo.start_pid = None;
            let plain = if was_fetching {
                "could not download Forgejo — check the internet connection; the next start tries again"
            } else if retry {
                "could not start Forgejo — it tries again"
            } else {
                "could not start Forgejo — press r to try again"
            };
            inner.forgejo.state = ForgejoState::Failed {
                reason: plain.to_owned(),
            };
            if retry {
                inner.forgejo.failures = inner.forgejo.failures.saturating_add(1);
                inner.forgejo.retry_at =
                    Some(Instant::now() + backoff_for_restart(inner.forgejo.failures));
                inner.forgejo.start_requested = true;
            } else {
                inner.forgejo.start_requested = false;
            }
            inner.note(plain);
        }
    }
}

fn ensure_admin_token(
    inner: &Arc<Mutex<Inner>>,
    binary: &Path,
    data_root: &Path,
    admin_login: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Secret, String> {
    let credentials = inner
        .lock()
        .map(|inner| inner.credentials.clone())
        .map_err(|_| "the engine state is poisoned".to_owned())?;
    if let Ok(Some(token)) = credentials::forgejo_admin_token(&credentials) {
        let url = inner
            .lock()
            .map(|inner| inner.forgejo.url.clone())
            .unwrap_or_default();
        if let Ok(client) = ForgejoClient::new(&url, token.clone(), DEFAULT_TIMEOUT) {
            if let Ok(viewer) = client.viewer() {
                if viewer.login == admin_login && viewer.is_admin {
                    return Ok(token);
                }
            }
        }
    }
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    // Register the helper as in flight under the state lock before the child exists.
    // `shutdown` sees either this count or the child's pid, so it cannot finish
    // between the stop check and the spawn and leave the helper behind.
    {
        let mut inner = lock_inner(inner);
        if inner.stopping {
            return Err("the start was called off".to_owned());
        }
        inner.forgejo.helper_inflight += 1;
    }
    let spawned = Arc::new(AtomicU32::new(0));
    let record_inner = Arc::clone(inner);
    let record_spawned = Arc::clone(&spawned);
    let on_spawn = move |pid: u32| {
        record_spawned.store(pid, Ordering::SeqCst);
        let mut inner = lock_inner(&record_inner);
        inner.forgejo.helper_pids.push(pid);
    };
    let result = bundled_forgejo::generate_admin_token_checked(
        binary,
        data_root,
        admin_login,
        cancelled,
        on_spawn,
    );
    // The helper's own job has waited for it, so this removes a pid that is already
    // reaped. The count is cleared on every path, including a failed or called-off
    // helper, so `shutdown`'s final wait can end.
    {
        let mut inner = lock_inner(inner);
        inner.forgejo.helper_inflight = inner.forgejo.helper_inflight.saturating_sub(1);
        let pid = spawned.load(Ordering::SeqCst);
        if pid != 0 {
            inner.forgejo.helper_pids.retain(|kept| *kept != pid);
        }
    }
    let token = result?;
    credentials::record_secret(&credentials, None, "forgejo_admin_token", token.expose())
        .map_err(|error| format!("the administrator token could not be kept: {error}"))?;
    Ok(token)
}

fn forgejo_provision_job(inner: Arc<Mutex<Inner>>, options: EngineOptions) {
    let (url, token, admin_login) = match inner.lock() {
        Ok(inner) => (
            inner.forgejo.url.clone(),
            inner.forgejo.admin_token.clone(),
            inner.forgejo.admin_login.clone(),
        ),
        Err(poisoned) => {
            let inner = poisoned.into_inner();
            (
                inner.forgejo.url.clone(),
                inner.forgejo.admin_token.clone(),
                inner.forgejo.admin_login.clone(),
            )
        }
    };
    let binary = bundled_forgejo::binary_path(&options.data_root);
    let token_cancel = {
        let inner = Arc::clone(&inner);
        move || lock_inner(&inner).stopping
    };
    let token = token.or_else(|| {
        ensure_admin_token(
            &inner,
            &binary,
            &options.data_root,
            &admin_login,
            &token_cancel,
        )
        .ok()
    });
    let result = match token.as_ref() {
        Some(token) => ForgejoClient::new(&url, token.clone(), DEFAULT_TIMEOUT)
            .and_then(|client| bundled_forgejo::provision(&client)),
        None => Err(ForgejoError::MissingToken),
    };
    if let Ok(mut inner) = inner.lock() {
        inner.forgejo.provision_inflight = false;
        match result {
            Ok(repositories) => {
                inner.forgejo.admin_token = token;
                inner.forgejo.repositories = repositories;
                inner.forgejo.provisioned = true;
                inner.forgejo.provision_retry_at = None;
                inner.forgejo.repositories_refreshed_at = Some(Instant::now());
                inner.note("Forgejo is ready");
            }
            Err(_error) => {
                inner.forgejo.provisioned = false;
                inner.forgejo.provision_retry_at = Some(Instant::now() + PROVISION_RETRY);
                inner.note("Forgejo is still getting ready");
            }
        }
    }
}

fn repository_refresh_job(inner: Arc<Mutex<Inner>>) {
    let (url, token) = match inner.lock() {
        Ok(inner) => (inner.forgejo.url.clone(), inner.forgejo.admin_token.clone()),
        Err(poisoned) => {
            let inner = poisoned.into_inner();
            (inner.forgejo.url.clone(), inner.forgejo.admin_token.clone())
        }
    };
    let Some(token) = token else {
        if let Ok(mut inner) = inner.lock() {
            inner.forgejo.repositories_inflight = false;
        }
        return;
    };
    let result = ForgejoClient::new(&url, token, DEFAULT_TIMEOUT)
        .and_then(|client| client.organisation_repositories(bundled_forgejo::ORGANISATION));
    if let Ok(mut inner) = inner.lock() {
        inner.forgejo.repositories_inflight = false;
        // A failed fetch is retried on the next tick rather than after a full poll
        // interval: the board must not keep an old count for two intervals because
        // one request failed (W31-8).
        inner.forgejo.repositories_refreshed_at = result.is_ok().then(Instant::now);
        if let Ok(repositories) = result {
            // The administration API's own repository list is polled; a name that
            // disappeared or appeared between two polls is an event on the board, so
            // a destroyed repository is never traceless.
            let before: Vec<String> = inner
                .forgejo
                .repositories
                .iter()
                .map(|repository| repository.full_name.clone())
                .collect();
            let after: Vec<String> = repositories
                .iter()
                .map(|repository| repository.full_name.clone())
                .collect();
            let seen = repository_events(&before, &after);
            for event in seen {
                inner.forgejo.events.push_back(event);
            }
            while inner.forgejo.events.len() > 5 {
                inner.forgejo.events.pop_front();
            }
            if before != after {
                if let Some(event) = inner.forgejo.events.back().cloned() {
                    inner.settings.forgejo.last_event = Some(event.clone());
                    let _ = inner.save_settings();
                    inner.note(event);
                }
            }
            inner.forgejo.repositories = repositories;
        }
    }
}

/// The repository changes between two administration-API polls, as the board's
/// events: a deleted repository is never traceless, and a new one is announced.
fn repository_events(before: &[String], after: &[String]) -> Vec<String> {
    let mut events = Vec::new();
    for name in before {
        if !after.contains(name) {
            events.push(format!("`{name}` was deleted"));
        }
    }
    // The first list is the provisioning result, not a set of creations: only a
    // later poll can know that a repository is new.
    if !before.is_empty() {
        for name in after {
            if !before.contains(name) {
                events.push(format!("`{name}` was created"));
            }
        }
    }
    events
}

fn provision_agent_job(inner: Arc<Mutex<Inner>>, name: String) {
    if let Ok(mut guard) = inner.lock()
        && removal_marked(&guard.options.layout, &name)
    {
        if let Some(index) = guard.agent_index(&name) {
            guard.agents[index].provision_inflight = false;
        }
        return;
    }
    let (url, token) = match inner.lock() {
        Ok(inner) => (inner.forgejo.url.clone(), inner.forgejo.admin_token.clone()),
        Err(poisoned) => {
            let inner = poisoned.into_inner();
            (inner.forgejo.url.clone(), inner.forgejo.admin_token.clone())
        }
    };
    let credentials = inner
        .lock()
        .map(|inner| inner.credentials.clone())
        .unwrap_or_default();
    let result = match token {
        Some(_) => identity::manage(&url, &name, &credentials)
            .map_err(|error| sentence(&error))
            .and_then(|identity| {
                let admin = credentials::forgejo_admin_token(&credentials)
                    .ok()
                    .flatten()
                    .ok_or_else(|| "the administrator token is not there".to_owned())?;
                let client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT)
                    .map_err(|error| sentence(&error))?;
                bundled_forgejo::join_team(&client, &identity.login)
                    .map_err(|error| sentence(&error))?;
                Ok(identity)
            }),
        None => Err("the Forgejo administrator token is not ready".to_owned()),
    };
    if let Ok(mut inner) = inner.lock() {
        let Some(index) = inner.agent_index(&name) else {
            return;
        };
        inner.agents[index].provision_inflight = false;
        match result {
            Ok(identity) => {
                inner.agents[index].login = Some(identity.login.clone());
                inner.agents[index].email = Some(identity.email.clone());
                inner.agents[index].provisioning = Provisioning::Ready;
                inner.agents[index].provision_retry_at = None;
                inner.note(format!("`{name}` is connected as `{}`", identity.login));
            }
            Err(reason) => {
                inner.agents[index].provisioning = Provisioning::Failed {
                    reason: reason.clone(),
                };
                inner.agents[index].provision_retry_at = Some(Instant::now() + PROVISION_RETRY);
                inner.note(format!("`{name}` could not be connected — it tries again"));
            }
        }
    }
}

fn session_job(
    inner: Arc<Mutex<Inner>>,
    request: SessionRequest,
    spawner: Arc<dyn SessionSpawner>,
    clock: SharedClock,
    generation: u64,
) {
    let prepared: Result<PreparedSession, String> = spawner.prepare(&request);
    // The slow half is over: no child exists yet. A stop or shutdown that happened
    // during it must win before anything is spawned.
    {
        let mut guard = lock_inner(&inner);
        let Some(index) = guard.agent_index(&request.agent) else {
            // `prepared` is dropped here; its owner guard records the ending.
            return;
        };
        if guard.stopping
            || guard.generation != generation
            || guard.agents[index].desired != Desired::Started
            || !guard.agents[index].starting
        {
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            // `prepared` is dropped here; its owner guard records the ending.
            return;
        }
    }
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            let now = clock.now();
            let mut guard = lock_inner(&inner);
            let Some(index) = guard.agent_index(&request.agent) else {
                return;
            };
            if guard.stopping
                || guard.generation != generation
                || guard.agents[index].desired != Desired::Started
            {
                guard.agents[index].starting = false;
                guard.agents[index].starting_pid = None;
                return;
            }
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            let started = guard.agents[index].starting_since.unwrap_or(now);
            let state_root = guard.options.layout.state_root().to_path_buf();
            let agent = &mut guard.agents[index];
            finish_session(
                agent,
                SessionEnd::Failed { reason },
                now,
                started,
                &state_root,
                Some(&request.run_dir),
            );
            return;
        }
    };
    // Spawn without the state lock. The spawner reports the pid as soon as the child
    // exists, so a shutdown during a spawner's own wait can signal it. Recording the
    // pid takes the lock only for the short store.
    let record_inner = Arc::clone(&inner);
    let record_agent = request.agent.clone();
    let mut on_spawn = move |pid: u32| {
        let mut guard = lock_inner(&record_inner);
        if let Some(index) = guard.agent_index(&record_agent) {
            guard.agents[index].starting_pid = Some(pid);
        }
    };
    let started = prepared
        .guard
        .start(&mut on_spawn, |run_dir, workspace, owner_on_spawn| {
            spawner.spawn_with(run_dir, workspace, owner_on_spawn)
        });
    let now = clock.now();
    let mut guard = lock_inner(&inner);
    let Some(index) = guard.agent_index(&request.agent) else {
        // The agent was removed while its spawn was in flight. This job still owns
        // the child it created: stop it, then settle the run so its durable ending
        // and its identity handles are put in the correct order. If settlement
        // fails, the handles stay for the next open, which scans every agent
        // directory on disk, not only the current settings.
        drop(guard);
        if let Ok(mut started) = started {
            let proposed = if run::terminal_recorded(&request.run_dir).unwrap_or(true) {
                run::FirstEnding::Terminal
            } else {
                run::FirstEnding::Stop
            };
            let first_ending = match started.request_stop(proposed) {
                Ok(first_ending) => first_ending,
                Err(error) => {
                    // The child still has to be stopped, but without a valid fact
                    // this is an uncertain ending, not a clean stop.
                    let reason = started.stop_uncertain(&error, Duration::from_secs(1));
                    let mut guard = lock_inner(&inner);
                    if let Some(index) = guard.agent_index(&request.agent) {
                        guard.agents[index].leftover = Some(reason.clone());
                        guard.agents[index].leftover_targets =
                            vec![RecoveryTarget::Run(request.run_dir.clone())];
                        guard.agents[index].session = SessionState::Blocked { reason };
                        guard.agents[index].next_at = None;
                    }
                    return;
                }
            };
            let _ = started.request_finish(
                run::RunOutcome::Stopped,
                Some("the run was called off when its agent was removed".to_owned()),
                Duration::from_secs(1),
                Some("the run was called off when its agent was removed".to_owned()),
                first_ending,
            );
        }
        return;
    };
    let called_off = guard.stopping
        || guard.generation != generation
        || guard.agents[index].desired != Desired::Started
        || !guard.agents[index].starting;
    if called_off {
        guard.agents[index].starting = false;
        guard.agents[index].starting_pid = None;
        let stopping = guard.stopping;
        drop(guard);
        if let Ok(mut started) = started {
            // This job owns the child it has just spawned. Stop and reap it; the
            // shutdown that called it off may have signalled the group already.
            let proposed = if run::terminal_recorded(&request.run_dir).unwrap_or(true) {
                run::FirstEnding::Terminal
            } else {
                run::FirstEnding::Stop
            };
            let first_ending = match started.request_stop(proposed) {
                Ok(first_ending) => first_ending,
                Err(error) => {
                    let reason = started.stop_uncertain(&error, Duration::from_secs(1));
                    let mut guard = lock_inner(&inner);
                    if let Some(index) = guard.agent_index(&request.agent) {
                        guard.agents[index].leftover = Some(reason.clone());
                        guard.agents[index].leftover_targets =
                            vec![RecoveryTarget::Run(request.run_dir.clone())];
                        guard.agents[index].session = SessionState::Blocked { reason };
                        guard.agents[index].next_at = None;
                    }
                    return;
                }
            };
            let reason = if stopping {
                "the run was stopped when the program closed"
            } else {
                "the run was stopped before it became current"
            };
            match started.request_finish(
                run::RunOutcome::Stopped,
                Some(reason.to_owned()),
                Duration::from_secs(1),
                Some(reason.to_owned()),
                first_ending,
            ) {
                Ok(_) => {}
                Err(reason) => {
                    let mut guard = lock_inner(&inner);
                    if let Some(index) = guard.agent_index(&request.agent) {
                        guard.agents[index].leftover = Some(reason.clone());
                        guard.agents[index].leftover_targets =
                            vec![RecoveryTarget::Run(request.run_dir.clone())];
                        guard.agents[index].session = SessionState::Blocked { reason };
                        guard.agents[index].next_at = None;
                    }
                }
            }
        }
        return;
    }
    let started_at = guard.agents[index].starting_since.unwrap_or(now);
    match started {
        Ok(started) => {
            let _pid = started.pid();
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            guard.agents[index].running = Some(RunningState {
                run_dir: request.run_dir.clone(),
                started: started_at,
                run: started,
                stopping: false,
                stop_deadline: None,
                stop_requested: false,
                timed_out: false,
                first_ending: None,
                first_ending_error: None,
            });
            guard.agents[index].newest_run = Some(request.run_dir.clone());
            guard.agents[index].activity_refresh_at = None;
            // A new run is not the previous one: its failure, its tool and its
            // activity are read from this run's own records. Keeping the old rows
            // here made a previous refusal look like the current run's result
            // (F7); the last-work block is deliberately kept until new work comes.
            guard.agents[index].last_failure = None;
            guard.agents[index].last_error = None;
            guard.agents[index].activity = None;
            guard.agents[index].activity_at = None;
            guard.agents[index].current_tool = None;
            guard.agents[index].current_tool_since = None;
            guard.agents[index].no_assigned_work = false;
            guard.agents[index].session = SessionState::Running {
                number: guard.agents[index].session_number,
                since: SystemTime::now(),
            };
        }
        Err(reason) => {
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            let _ = run::request_failed_finish(&request.run_dir, &request.run_id, &reason);
            let state_root = guard.options.layout.state_root().to_path_buf();
            let agent = &mut guard.agents[index];
            finish_session(
                agent,
                SessionEnd::Failed { reason },
                now,
                started_at,
                &state_root,
                Some(&request.run_dir),
            );
        }
    }
}

/// The post-removal line, short enough for an 80-column board (F16).
fn removed_note(name: &str) -> String {
    format!("`{name}` was removed; login deactivated, history kept")
}

fn remove_agent_job(inner: Arc<Mutex<Inner>>, name: String) {
    let marker = match inner.lock() {
        Ok(inner) => write_removal_marker(&inner, &name),
        Err(poisoned) => write_removal_marker(&poisoned.into_inner(), &name),
    };
    if let Err(reason) = marker {
        if let Ok(mut inner) = inner.lock() {
            if let Some(index) = inner.agent_index(&name) {
                // Nothing was cancelled or changed. Do not leave a failed
                // provisioning/blocked state behind: the agent is exactly as it was
                // before the Remove request, except that it is not auto-retried.
                inner.agents[index].removal_pending = false;
                // The notice is bounded at 600 bytes and repeats the marker path in
                // `reason`. Put the fact a person must act on before that path so a
                // long state root can shorten the reason but never lose the recovery.
                inner.note(format!(
                    "`{name}` was stopped and left stopped, but it could not be removed; its Forgejo sign-in and account were not changed; start it again or retry Remove: {reason}"
                ));
            }
        }
        return;
    }
    let (url, admin_token, credentials, login, token_name) = match inner.lock() {
        Ok(inner) => {
            let Some(index) = inner.agent_index(&name) else {
                return;
            };
            let agent = &inner.agents[index];
            let token_name = credentials::recorded_agent_token_name(&inner.credentials, &name)
                .ok()
                .flatten()
                .unwrap_or_else(|| format!("gewu-{name}"));
            (
                inner.forgejo.url.clone(),
                inner.forgejo.admin_token.clone(),
                inner.credentials.clone(),
                agent.login.clone(),
                token_name,
            )
        }
        Err(poisoned) => {
            let inner = poisoned.into_inner();
            let token_name = credentials::recorded_agent_token_name(&inner.credentials, &name)
                .ok()
                .flatten()
                .unwrap_or_else(|| format!("gewu-{name}"));
            (
                inner.forgejo.url.clone(),
                inner.forgejo.admin_token.clone(),
                inner.credentials.clone(),
                inner
                    .agents
                    .iter()
                    .find(|agent| agent.name == name)
                    .and_then(|agent| agent.login.clone()),
                token_name,
            )
        }
    };
    let result = (|| -> Result<(), String> {
        if let Some(login) = &login {
            let token = admin_token
                .ok_or_else(|| "the Forgejo administrator token is not ready".to_owned())?;
            let client = ForgejoClient::new(&url, token, DEFAULT_TIMEOUT)
                .map_err(|error| sentence(&error))?;
            // The recorded name is the token that is live now; the deterministic first
            // name covers an identity settled before names were recorded.
            let mut names = vec![token_name.clone()];
            let first = format!("gewu-{name}");
            if !names.contains(&first) {
                names.push(first);
            }
            for token_name in names {
                client
                    .delete_token_for(login, &token_name)
                    .map_err(|error| sentence(&error))?;
            }
            bundled_forgejo::leave_team(&client, login).map_err(|error| sentence(&error))?;
            // The account row and its history stay; `active = false` is what stops a
            // web sign-in as well as every token (P4).
            client
                .deactivate_user(login)
                .map_err(|error| sentence(&error))?;
        }
        credentials::remove_agent_token(&credentials, &name)
            .map_err(|error| format!("the local token could not be removed: {error}"))?;
        Ok(())
    })();
    if let Ok(mut inner) = inner.lock() {
        let Some(index) = inner.agent_index(&name) else {
            return;
        };
        match result {
            Ok(()) => {
                match inner.commit_settings(|settings| {
                    settings.remove_agent(&name);
                    Ok(())
                }) {
                    Ok(()) => {
                        inner.agents.remove(index);
                        clear_removal_marker(&inner.options.layout, &name);
                        inner.note(removed_note(&name));
                    }
                    Err(error) => {
                        let reason = format!(
                            "`{name}`'s Forgejo sign-in and tokens were already cancelled, but the removal could not be saved; it is still on the board and Remove can be retried ({error})"
                        );
                        inner.agents[index].removal_pending = false;
                        inner.agents[index].removal_retry_at =
                            Some(Instant::now() + PROVISION_RETRY);
                        inner.agents[index].login = None;
                        inner.agents[index].provisioning = Provisioning::Failed {
                            reason: reason.clone(),
                        };
                        inner.agents[index].session = SessionState::Blocked {
                            reason: reason.clone(),
                        };
                        inner.note(reason);
                    }
                }
            }
            Err(_reason) => {
                inner.agents[index].removal_pending = false;
                inner.agents[index].removal_retry_at = Some(Instant::now() + PROVISION_RETRY);
                inner.note(format!(
                    "`{name}` was kept — its login could not be cancelled"
                ));
            }
        }
    }
}

/// Restore the model key after a failed endpoint settings commit. The key was
/// written before the copy was committed; a settings failure must not pair the old
/// endpoint with the new provider's key.
fn restore_endpoint_key(
    inner: &Inner,
    previous: Option<credentials::Secret>,
    error: String,
) -> String {
    let restored = match previous {
        Some(secret) => {
            credentials::record_secret(&inner.credentials, None, "model_key", secret.expose())
        }
        None => credentials::remove_secret(&inner.credentials, None, "model_key"),
    };
    match restored {
        Ok(()) => {
            format!("the connection was not saved; the previous model key was restored ({error})")
        }
        Err(restore_error) => format!(
            "the connection was not saved; the previous model key could not be restored ({error}); the key restore also failed ({restore_error})"
        ),
    }
}

/// The private, durable record that this agent is being removed. It lives beside
/// the run records, not in settings, because the failure it must survive is a
/// settings write failure.
fn removal_marker_path(layout: &Layout, name: &str) -> Option<PathBuf> {
    layout
        .agent(name)
        .ok()
        .map(|paths| paths.state_dir().join("removal-pending"))
}

fn removal_marked(layout: &Layout, name: &str) -> bool {
    removal_marker_path(layout, name).is_some_and(|path| path.exists())
}

fn write_removal_marker(inner: &Inner, name: &str) -> Result<PathBuf, String> {
    let paths = inner
        .options
        .layout
        .agent(name)
        .map_err(|error| format!("the agent's state directory is not available: {error}"))?;
    fs::create_dir_all(paths.state_dir()).map_err(|error| {
        format!(
            "the removal marker directory {} cannot be created: {error}; nothing was cancelled",
            paths.state_dir().display()
        )
    })?;
    set_mode(paths.state_dir(), PRIVATE_DIR_MODE).map_err(|error| {
        format!(
            "the removal marker directory {} cannot be made private: {error}; nothing was cancelled",
            paths.state_dir().display()
        )
    })?;
    let marker = paths.state_dir().join("removal-pending");
    crate::mode::write_private(&marker, b"removal pending\n").map_err(|error| {
        format!(
            "the removal could not be recorded at {}: {error}; nothing was cancelled",
            marker.display()
        )
    })?;
    Ok(marker)
}

fn clear_removal_marker(layout: &Layout, name: &str) {
    if let Some(marker) = removal_marker_path(layout, name) {
        let _ = fs::remove_file(marker);
    }
}

fn endpoint_test_job(
    inner: Arc<Mutex<Inner>>,
    endpoint: String,
    model: String,
    credentials: PathBuf,
) {
    let result: Result<crate::model::Reply, String> = match credentials::model_key(&credentials, "")
    {
        Ok(api_key) => {
            let config = crate::model::ModelConfig {
                endpoint: endpoint.clone(),
                model: model.clone(),
                api_key,
                timeout: crate::model::PROBE_TIMEOUT,
            };
            crate::model::probe(&config).map_err(|error| plain_endpoint_failure(&error, &model))
        }
        Err(_) => Err("the API key was refused".to_owned()),
    };
    if let Ok(mut inner) = inner.lock() {
        inner.endpoint_test = match result {
            Ok(_) => EndpointTest::Passed {
                answer: "the model answered".to_owned(),
            },
            Err(reason) => EndpointTest::Failed { reason },
        };
    }
}

/// One endpoint failure in the words the model screen uses. The suffix check and the
/// empty-key check say nothing was sent; a provider's refusal names its status and
/// the first clause of its own message. No raw body or engine word reaches the person.
fn plain_endpoint_failure(error: &crate::model::ModelError, model: &str) -> String {
    crate::model::failure_words(error, model)
}

fn sentence(error: &impl std::fmt::Display) -> String {
    crate::forgejo_tools::sentence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use crate::engine::clock::FakeClock;
    use crate::test_support::scratch_root;
    use crate::test_wait::{recv_within, wait_for, wait_until};

    fn test_options(root: &Path) -> EngineOptions {
        let layout = Layout::rooted_at(root.to_path_buf()).expect("the test layout is absolute");
        EngineOptions {
            layout,
            data_root: root.join("data"),
            product_binary: PathBuf::from("/bin/true"),
            session_timeout: DEFAULT_SESSION_TIMEOUT,
            stop_grace: DEFAULT_STOP_GRACE,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            auto_start_forgejo: false,
            confinement: crate::confinement::Method::None,
            forgejo_ready: true,
            forgejo_starter: None,
        }
    }

    fn write_ready(root: &Path, agents: &[(&str, Desired)]) -> Settings {
        let layout = Layout::rooted_at(root.to_path_buf()).expect("the test layout is absolute");
        layout.ensure_root().expect("root");
        CredentialsFile::prepare(&layout.credentials_file()).expect("credentials");
        credentials::record_secret(&layout.credentials_file(), None, "model_key", "gw-test-key")
            .expect("key");
        let mut settings = Settings::default();
        settings.endpoint.url = "http://127.0.0.1:9/v1/chat/completions".to_owned();
        settings.endpoint.model = "lab-chat-1".to_owned();
        settings.terms_acknowledged = true;
        settings.terms_version = TERMS_VERSION;
        for (name, desired) in agents {
            settings.add_agent(name, *desired).expect("agent");
            let paths = layout.ensure_agent(name).expect("paths");
            fs::write(paths.instruction(), "do the work\n").expect("instruction");
        }
        settings
            .save(&settings::path_in(layout.root()))
            .expect("settings");
        settings
    }

    fn open_test_engine(
        root: &Path,
        settings: Settings,
        clock: Arc<FakeClock>,
        fake: Arc<FakeSpawner>,
    ) -> Engine {
        open_for_test(test_options(root), clock, fake, settings).expect("the test engine opens")
    }

    fn tick(engine: &mut Engine, clock: &FakeClock, by: Duration) {
        clock.advance(by);
        engine.tick();
    }

    /// Wait for the background start job to land, without advancing the fake clock.
    fn wait_started(fake: &FakeSpawner, count: usize) {
        wait_until("the session start", || fake.counts().1 >= count);
        assert_eq!(fake.counts().1, count, "the session did not start");
    }

    #[test]
    fn c6_the_recovery_claim_names_the_agent_and_the_run() {
        let claim = RecoveryClaim {
            agent: Some("agent-2".to_owned()),
            run_id: Some("run-1700000200000-0007".to_owned()),
            forgejo: true,
        };
        let message = claim.message();
        assert!(message.contains("agent-2"), "{message}");
        assert!(message.contains("run-1700000200000-0007"), "{message}");
        assert!(message.contains("Forgejo"), "{message}");
        assert!(!message.contains("earlier run"), "{message}");
        let only_forgejo = RecoveryClaim {
            agent: None,
            run_id: None,
            forgejo: true,
        };
        assert_eq!(only_forgejo.message(), "stopping Forgejo…");
    }

    #[test]
    fn an_earlier_setup_is_left_as_it_was_and_says_so_once() {
        let root = scratch_root("engine-earlier-config");
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        layout.ensure_root().expect("the root");
        fs::write(
            layout.config_file(),
            "[forgejo]\nurl = \"https://example.org\"\n",
        )
        .expect("the old configuration is written");
        let mut engine = open_test_engine(
            &root,
            Settings::default(),
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        assert_eq!(
            engine.board().notice.as_deref(),
            Some("an earlier setup was found and left as it was")
        );
        // It is one line on the first view, then the normal screen takes over.
        engine.tick();
        assert!(engine.board().notice.is_none());
        assert_eq!(
            fs::read_to_string(layout.config_file()).expect("the old file is still there"),
            "[forgejo]\nurl = \"https://example.org\"\n"
        );
    }

    #[test]
    fn starting_an_agent_without_an_instruction_says_so_and_does_not_run() {
        let root = scratch_root("engine-start-no-instruction");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        let paths = layout.agent("agent-1").expect("paths");
        fs::remove_file(paths.instruction()).expect("the instruction is removed");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        tick(&mut engine, &clock, Duration::ZERO);
        let board = engine.board();
        assert_eq!(
            board.notice.as_deref(),
            Some("not started — write an instruction first")
        );
        assert!(matches!(
            board.agents[0].session,
            SessionState::Blocked { .. }
        ));
        assert_eq!(fake.counts().1, 0, "a run started without an instruction");
    }

    #[test]
    fn repository_polls_turn_into_created_and_deleted_events() {
        let before = vec!["team/workspace".to_owned(), "team/scratch".to_owned()];
        let after = vec!["team/workspace".to_owned(), "team/website".to_owned()];
        assert_eq!(
            repository_events(&before, &after),
            vec!["`team/scratch` was deleted", "`team/website` was created"]
        );
        // The provisioning result is not a creation event.
        assert!(repository_events(&[], &after).is_empty());
        assert!(repository_events(&after, &after).is_empty());
    }

    #[test]
    fn an_unconfined_host_says_so_in_the_terms_and_on_the_board() {
        let root = scratch_root("engine-unconfined-words");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let board = engine.board();
        assert!(!board.confined, "the board pretends the run is confined");
        assert_eq!(
            board.terms.last().map(String::as_str),
            Some("Runs are not confined on this computer."),
            "the sixth warning is missing: {:?}",
            board.terms
        );
        assert_eq!(board.terms.len(), TERMS.len() + 1);
    }

    #[test]
    fn a_symlinked_state_root_inside_a_workspace_is_unconfined_on_the_board() {
        // B9-R: `detect` must compare canonical paths. The state root's name is
        // beside the configuration root, but its target is inside an agent's
        // workspace, which the namespace mechanisms bind back. The board must say so
        // with the sixth sentence, not claim a boundary.
        let root = scratch_root("engine-b9r-symlink");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let target = root.join("agents/agent-1/workspace/state");
        fs::create_dir_all(&target).expect("a state root inside the workspace");
        let state = root.join("state-link");
        std::os::unix::fs::symlink(&target, &state).expect("the state symlink");
        let layout = Layout::rooted_at(root.clone())
            .expect("the test layout")
            .with_state_root(state)
            .expect("the state root");
        let mut options =
            EngineOptions::new(layout, root.join("data")).expect("the engine options");
        assert_eq!(
            options.confinement,
            crate::confinement::Method::None,
            "detect claimed a boundary for a symlinked state root inside the workspace"
        );
        options.auto_start_forgejo = false;
        options.forgejo_ready = true;
        options.forgejo_starter = None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let board = engine.board();
        assert!(!board.confined, "the board pretends the run is confined");
        assert_eq!(
            board.terms.last().map(String::as_str),
            Some("Runs are not confined on this computer."),
            "the sixth warning is missing: {:?}",
            board.terms
        );
        assert_eq!(board.terms.len(), TERMS.len() + 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_workspace_over_the_config_parent_is_unconfined_on_the_board() {
        // B9-R-R: `detect` must enumerate the real workspaces. `<config>/agents/
        // agent-1/workspace` is a link to the configuration root's parent; the lexical
        // workspace is inside `<config>/agents`, which the old check did see, but the
        // *resolved* workspace contains the configuration root and every mechanism
        // would hand it back. The board must say so with the sixth sentence.
        let base = scratch_root("engine-b9rr-workspace");
        let config = base.join("config");
        let settings = write_ready(&config, &[("agent-1", Desired::Stopped)]);
        let workspace = config.join("agents/agent-1/workspace");
        fs::remove_dir_all(&workspace).expect("the created workspace");
        std::os::unix::fs::symlink(&base, &workspace).expect("the workspace symlink");
        let mut options = EngineOptions::new(
            Layout::rooted_at(config.clone()).expect("layout"),
            base.join("data"),
        )
        .expect("the engine options");
        assert_eq!(
            options.confinement,
            crate::confinement::Method::None,
            "detect claimed a boundary for a workspace symlinked over the config parent"
        );
        options.auto_start_forgejo = false;
        options.forgejo_ready = true;
        options.forgejo_starter = None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let board = engine.board();
        assert!(!board.confined, "the board pretends the run is confined");
        assert_eq!(
            board.terms.last().map(String::as_str),
            Some("Runs are not confined on this computer."),
            "the sixth warning is missing: {:?}",
            board.terms
        );
        assert_eq!(board.terms.len(), TERMS.len() + 1);
        // The session request that follows carries `none`, not the stale method, so
        // the run plan does not promise a boundary the layout cannot keep.
        let mut inner = engine.inner.lock().expect("the engine");
        let request = inner.session_request(0).expect("a session request");
        assert_eq!(request.confinement, crate::confinement::Method::None);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_run_time_refusal_marker_takes_the_confined_claim_off_the_board() {
        // B9-R-R: even a plan written before the layout changed must not fall back
        // silently. The child's marker is read by the poll and the board stops
        // claiming confinement, with the sixth sentence and a notice.
        let root = scratch_root("engine-b9rr-refusal");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::Landlock;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let run_dir = root.join("state/agent-1/run-1");
        fs::create_dir_all(&run_dir).expect("the run directory");
        crate::run::write_confinement_refusal(
            &run_dir,
            "a hidden directory lies inside the run's workspace",
        )
        .expect("the refusal marker");
        assert!(engine.board().confined, "the test starts confined");
        {
            let mut inner = engine.inner.lock().expect("the engine");
            inner.observe_confinement_refusal(&run_dir);
        }
        let board = engine.board();
        assert!(!board.confined, "the refusal left the claim on the board");
        assert_eq!(
            board.terms.last().map(String::as_str),
            Some("Runs are not confined on this computer."),
            "the sixth warning is missing: {:?}",
            board.terms
        );
        assert!(
            board
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Runs are not confined on this computer")),
            "the refusal is not on the board: {:?}",
            board.notice
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_confined_session_plan_names_the_roots_to_hide() {
        let root = scratch_root("engine-confined-plan");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::Landlock;
        let engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let mut inner = engine.inner.lock().expect("the engine");
        let request = inner.session_request(0).expect("a session request");
        assert_eq!(request.confinement, crate::confinement::Method::Landlock);
        assert!(
            request
                .hide
                .iter()
                .any(|path| path == inner.options.layout.root())
        );
        assert!(
            request
                .hide
                .iter()
                .any(|path| path == inner.options.layout.state_root())
        );
        assert!(
            request
                .hide
                .iter()
                .any(|path| path == &inner.options.data_root)
        );
    }

    #[test]
    fn a_v0129_shaped_home_shows_one_upgrade_screen_and_records_the_version() {
        let root = scratch_root("engine-upgrade");
        let settings = write_ready(
            &root,
            &[("agent-1", Desired::Stopped), ("agent-2", Desired::Started)],
        );
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        // The earlier build's own records: a version, the old text screen's
        // config.toml, instructions (write_ready made two) and one run directory.
        last_seen::write(&root, "0.1.29").expect("the old version");
        fs::write(
            layout.config_file(),
            "[forgejo]\nurl = \"https://example.org\"\n",
        )
        .expect("the old configuration");
        let run_dir = layout
            .agent("agent-1")
            .expect("agent paths")
            .run_dir("run-1700000000000-0001");
        fs::create_dir_all(&run_dir).expect("the old run directory");
        let mut engine = open_test_engine(
            &root,
            settings,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        let view = engine.board();
        let upgrade = view.upgrade.expect("the upgrade screen is due");
        assert_eq!(upgrade.from_version, "0.1.29");
        assert_eq!(upgrade.to_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(upgrade.agents, 2);
        assert_eq!(upgrade.instructions, 2);
        assert_eq!(upgrade.runs, 1);
        assert!(upgrade.ignored_config, "the old config.toml was not named");
        assert_eq!(
            upgrade.kept_agents.len(),
            2,
            "the kept agents were not named"
        );
        assert_eq!(upgrade.kept_agents[0].name, "agent-1");
        assert!(!upgrade.kept_agents[0].program.is_empty());
        assert!(!upgrade.kept_agents[0].model.is_empty());
        assert_eq!(
            upgrade.release_url,
            format!(
                "https://github.com/{}/releases/tag/v{}",
                bundled_forgejo::REPOSITORY,
                env!("CARGO_PKG_VERSION")
            )
        );
        // The file now says this build, at open.
        assert_eq!(
            last_seen::read(&root).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // The board shows exactly one screen before the usual first screen.
        let mut board = crate::board::Board::new(engine);
        assert_eq!(board.screen(), crate::board::Screen::Upgrade);
        board.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        assert_eq!(board.screen(), crate::board::Screen::Home);
        // A clean exit writes the version again, even if the file was removed while
        // the board was open.
        let _ = fs::remove_file(root.join(last_seen::FILE));
        drop(board);
        assert_eq!(
            last_seen::read(&root).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn a_ready_agent_waits_for_forgejo_to_return_its_repositories() {
        let root = scratch_root("engine-first-run-context");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        // The agent was connected by an earlier visit: its login, token and rights
        // are recorded, so it is Ready without waiting for a new provisioning job.
        credentials::record_agent_identity(
            &layout.credentials_file(),
            "agent-1",
            Some(&Secret::new("agent-token")),
            "gewu-agent-1",
            "gewu-agent-1@agents.invalid",
            "managed",
            None,
        )
        .expect("the identity is recorded");
        credentials::record_agent_identity_rights(
            &layout.credentials_file(),
            "agent-1",
            identity::TOOL_RIGHTS_VERSION,
        )
        .expect("the rights are recorded");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut options = test_options(&root);
        options.forgejo_ready = false;
        let shared_clock: SharedClock = clock.clone();
        let shared_fake: Arc<dyn SessionSpawner> = fake.clone();
        let mut engine = open_for_test(options, shared_clock, shared_fake, settings)
            .expect("the test engine opens");

        tick(&mut engine, &clock, Duration::ZERO);
        assert_eq!(fake.counts().1, 0, "a session started before Forgejo");
        match &engine.board().agents[0].session {
            SessionState::Blocked { reason } => assert_eq!(reason, "waiting for Forgejo"),
            other => panic!("expected Blocked, got {other:?}"),
        }

        // Forgejo is up, but the provisioning job has not returned the organisation's
        // repositories yet. This is the gap after a reopen.
        {
            let mut inner = engine.inner.lock().expect("the engine is not poisoned");
            inner.forgejo.state = ForgejoState::Running;
            inner.forgejo.provisioned = false;
            inner.forgejo.provision_inflight = true;
        }
        tick(&mut engine, &clock, Duration::ZERO);
        assert_eq!(fake.counts().1, 0, "a shell-only session started");
        match &engine.board().agents[0].session {
            SessionState::Blocked { reason } => assert_eq!(reason, "waiting for Forgejo"),
            other => panic!("expected Blocked, got {other:?}"),
        }

        // Provisioning "finished" with no repositories: still no session, because the
        // run would have no Forgejo context.
        {
            let mut inner = engine.inner.lock().expect("the engine is not poisoned");
            inner.forgejo.provisioned = true;
            inner.forgejo.repositories.clear();
        }
        tick(&mut engine, &clock, Duration::ZERO);
        assert_eq!(fake.counts().1, 0, "a session started without repositories");

        // The repositories are back: the first run now starts with its plan and its
        // non-secret context, and the run declares the six Forgejo tools.
        {
            let mut inner = engine.inner.lock().expect("the engine is not poisoned");
            inner.forgejo.repositories = vec![Repository {
                full_name: "team/workspace".to_owned(),
                namespace: "team".to_owned(),
                name: "workspace".to_owned(),
                private: true,
                default_branch: Some("main".to_owned()),
                html_url: None,
                permissions: crate::forgejo::Permissions::default(),
            }];
        }
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        assert_eq!(
            fake.last_repositories()
                .iter()
                .map(|repository| repository.full_name.as_str())
                .collect::<Vec<_>>(),
            ["team/workspace"]
        );
        let run_dir = fake.last_run_dir().expect("the first run has a directory");
        assert!(run_dir.is_absolute(), "the run directory is not absolute");
        assert!(
            run_dir.join(run::PLAN_FILE).is_file(),
            "the first run has no plan.json"
        );
        let context = run::read_forgejo_context(&run_dir).expect("the first run has a context");
        assert_eq!(
            context
                .repositories
                .iter()
                .map(|repository| repository.full_name.as_str())
                .collect::<Vec<_>>(),
            ["team/workspace"]
        );
        let tools = crate::model::tools_with_forgejo(true);
        assert_eq!(
            tools.len(),
            7,
            "the first run does not declare shell plus the six Forgejo tools"
        );
        for name in crate::forgejo_tools::TOOL_NAMES {
            assert!(
                tools.iter().any(|tool| tool.function.name == name),
                "the first run does not declare {name}"
            );
        }
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    #[test]
    fn the_terms_are_the_five_sentences_the_board_shows() {
        assert_eq!(
            TERMS,
            [
                "Agents use this computer with your permissions.",
                "When runs are confined, they cannot read the product's keys, your home or another agent's files, and they start with a cleared environment.",
                "Each has its own login on your Forgejo.",
                "Started agents keep going until you stop them.",
                "Their work is sent to your model provider.",
            ]
        );
        // One plain sentence per item; the mechanism names live in the docs and in
        // the shell tool's description, which names the run's actual method (F10,
        // W31-9). The second item is true under every mechanism because it says
        // "when runs are confined".
        for term in TERMS {
            assert_eq!(
                term.matches(". ").count(),
                0,
                "the item holds more than one sentence: {term}"
            );
            assert!(term.ends_with('.'), "{term}");
            let lower = term.to_ascii_lowercase();
            for mechanism in ["landlock", "bwrap", "unshare", "gewu_confinement"] {
                assert!(
                    !lower.contains(mechanism),
                    "the consent line names {mechanism}: {term}"
                );
            }
        }
    }

    #[test]
    fn activities_are_written_out_in_plain_words() {
        assert_eq!(
            plain_activity("commented on team/workspace#12"),
            "commented on #12"
        );
        assert_eq!(plain_activity("commented#12"), "commented on #12");
        assert_eq!(plain_activity("opened #4"), "opened pull request #4");
        assert_eq!(plain_activity("updated #4"), "updated pull request #4");
        assert_eq!(plain_activity("did something else"), "did something else");
    }

    #[test]
    fn endpoint_failures_are_plain_sentences_without_http_or_engine_words() {
        use crate::model::ModelError;
        let cases = [
            (
                ModelError::BadUrl {
                    endpoint: "x".to_owned(),
                    problem: "y".to_owned(),
                },
                "wrong address",
            ),
            (
                ModelError::BadKey {
                    status: 401,
                    message: "Incorrect API key provided".to_owned(),
                },
                "the provider refused the API key",
            ),
            (ModelError::Timeout { secs: 15 }, "no answer within 15 s"),
            (
                ModelError::ModelNotFound {
                    model: "lab-chat-1".to_owned(),
                    status: 404,
                },
                "no model called lab-chat-1",
            ),
        ];
        for (error, wanted) in cases {
            let sentence = plain_endpoint_failure(&error, "lab-chat-1");
            assert!(sentence.contains(wanted), "{sentence}");
            let words = sentence.to_ascii_lowercase();
            for banned in ["endpoint", "session", "token", "2xx"] {
                assert!(!words.contains(banned), "{sentence} has {banned}");
            }
        }
    }

    #[test]
    fn a_second_engine_on_the_same_data_directory_is_refused() {
        let root = scratch_root("engine-lock");
        let first = open_test_engine(
            &root,
            Settings::default(),
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        let second = open_for_test(
            test_options(&root),
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            Settings::default(),
        );
        assert!(second.is_err(), "a second engine took the same data lock");
        drop(first);
        let third = open_for_test(
            test_options(&root),
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            Settings::default(),
        );
        assert!(
            third.is_ok(),
            "the lock was not released with the first engine"
        );
    }

    #[test]
    fn a_stopped_killed_or_crashed_run_leaves_no_private_temp() {
        let root = scratch_root("engine-run-temp");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);

        let make_temp = |pid: u32, run_dir: &Path| {
            let run_id = run_dir
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .expect("a run id");
            let temp = std::env::temp_dir().join(format!("gewu-run-{pid}-{run_id}"));
            fs::create_dir_all(temp.join("tmp")).expect("the run temp");
            fs::write(temp.join("tmp/askpass.toml"), b"synthetic").expect("the askpass file");
            temp
        };
        let first_run = fake.last_run_dir().expect("a run directory");
        let first_pid = fake.live()[0];
        let stopped = make_temp(first_pid, &first_run);
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut engine, &clock, Duration::ZERO);
        assert!(!stopped.exists(), "a stopped run left its askpass temp");

        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start again");
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 2);
        let crashed_run = fake.last_run_dir().expect("the crashed run directory");
        let crashed_pid = fake.live()[0];
        let crashed = make_temp(crashed_pid, &crashed_run);
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        assert!(!crashed.exists(), "a crashed run left its askpass temp");

        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start again");
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 3);
        let killed_run = fake.last_run_dir().expect("the killed run directory");
        let killed_pid = fake.live()[0];
        let killed = make_temp(killed_pid, &killed_run);
        engine.shutdown(Duration::ZERO);
        assert!(!killed.exists(), "a killed run left its askpass temp");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn adding_after_a_removal_takes_a_new_number_and_account() {
        let root = scratch_root("engine-add-after-remove");
        let mut settings = write_ready(
            &root,
            &[("agent-1", Desired::Stopped), ("agent-2", Desired::Stopped)],
        );
        // The remove job leaves the account and history; the settings lose the name.
        assert!(settings.remove_agent("agent-2"));
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        settings
            .save(&settings::path_in(layout.root()))
            .expect("settings");
        let mut engine = open_test_engine(
            &root,
            settings,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        engine.send(Command::AddAgent).expect("add");
        let board = engine.board();
        let names: Vec<&str> = board
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["agent-1", "agent-3"],
            "U5: + add reused the removed agent-2 number and account"
        );
        assert_eq!(
            board.agents[1].login.as_str(),
            "gewu-agent-3",
            "U5: the new agent did not get a new Forgejo account"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_cards_summary_prefers_the_newest_run_that_did_work() {
        let root = scratch_root("engine-last-work-result");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        let agent_paths = layout.agent("agent-1").expect("paths");
        let state = agent_paths.state_dir();
        // An older run that commented on an issue and finished with a summary.
        let older = state.join("run-1700000000000-0001");
        fs::create_dir_all(&older).expect("the older run");
        let mut transcript = crate::transcript::Transcript::open(&older).expect("transcript");
        transcript
            .append(&crate::transcript::Record::forgejo_result(
                1,
                1,
                crate::forgejo_tools::COMMENT,
                Some("team/workspace"),
                Some(1),
                "ok",
                None,
                Some("commented"),
            ))
            .expect("the comment record");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0001",
                run::RunOutcome::Succeeded,
                Some(0),
                1_000,
                Some("I fixed the flaky test".to_owned()),
            ))
            .expect("the older finish");
        // A newer check run that found the review already addressed.
        let newer = state.join("run-1700000000000-0002");
        fs::create_dir_all(&newer).expect("the newer run");
        let mut transcript = crate::transcript::Transcript::open(&newer).expect("transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0002",
                run::RunOutcome::NoWork,
                Some(0),
                500,
                Some("The review is already addressed".to_owned()),
            ))
            .expect("the newer finish");

        let engine = open_test_engine(
            &root,
            settings,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        let mut board = Board::new(engine);
        // A fresh root gets the one upgrade screen; continue to the board.
        for _ in 0..2 {
            if board.screen() == crate::board::Screen::Agent {
                break;
            }
            board.on_key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Enter,
            ));
        }
        assert_eq!(
            board.screen(),
            crate::board::Screen::Agent,
            "the Agent screen did not open"
        );
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        terminal
            .draw(|frame| board.draw(frame))
            .expect("the frame draws");
        let mut frame = String::new();
        let area = terminal.backend().buffer().area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.push_str(
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .expect("a cell")
                        .symbol(),
                );
            }
            frame.push('\n');
        }
        assert!(
            frame.contains("last work: comment on #1"),
            "the card did not keep the newest work:\n{frame}"
        );
        assert!(
            !frame.contains("result: The review is already addressed"),
            "the no-op's reason was presented as the work result:\n{frame}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_that_succeeds_restarts_after_a_visible_idle() {
        let root = scratch_root("engine-success");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let board = engine.board();
        assert!(matches!(
            &board.agents[0].session,
            SessionState::Restarting {
                last: SessionEnd::Succeeded,
                ..
            }
        ));
        tick(&mut engine, &clock, Duration::from_secs(4));
        assert_eq!(fake.counts().1, 1, "a session started before the 5 s idle");
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, 2);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    fn seed_a_warned_previous_run(root: &Path) {
        let old = root
            .join("state")
            .join("agent-1")
            .join("run-1700000000000-0007");
        fs::create_dir_all(&old).expect("the old run directory");
        let mut transcript = crate::transcript::Transcript::open(&old).expect("transcript");
        transcript
            .append(&crate::transcript::Record::forgejo_result(
                1,
                1,
                "forgejo_comment",
                Some("team/workspace"),
                Some(7),
                "ok",
                Some("http://127.0.0.1:3000/team/workspace/issues/7#c"),
                Some("commented"),
            ))
            .expect("the old comment");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0007",
                run::RunOutcome::Warned,
                Some(0),
                1_000,
                Some("the old warning".to_owned()),
            ))
            .expect("the old ending");
        fs::write(
            old.join(crate::run::FAILURE_FILE),
            b"write refused: HTTP 403 old refusal",
        )
        .expect("the old failure row");
    }

    fn assert_start_failure_provenance(root: &Path, fail_prepare: bool) {
        let settings = write_ready(root, &[("agent-1", Desired::Stopped)]);
        seed_a_warned_previous_run(root);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(root, settings, Arc::clone(&clock), Arc::clone(&fake));
        let before = engine.board().agents[0].clone();
        let old_failure = before.last_failure.clone();
        let old_activity = before.activity.clone();
        let old_activity_at = before.activity_at;
        assert_eq!(
            old_failure.as_deref(),
            Some("write refused: HTTP 403 old refusal")
        );
        let reason = if fail_prepare {
            fake.fail_prepare("the repository could not be cloned before the session: fixture");
            "the repository could not be cloned before the session: fixture"
        } else {
            fake.fail_spawn("the session process could not be started: fixture");
            "the session process could not be started: fixture"
        };
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("Start");
        wait_until("the current start failure", || {
            tick(&mut engine, &clock, Duration::ZERO);
            engine.board().agents[0]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("fixture"))
        });
        let after = engine.board().agents[0].clone();
        assert_eq!(
            after.last_failure, None,
            "the previous run's failure survived the new start failure"
        );
        assert!(
            after
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains(reason)),
            "the current start failure was not shown: {:?}",
            after.last_error
        );
        assert_eq!(
            after.activity, old_activity,
            "the previous activity was replaced by a failure attempt"
        );
        assert_eq!(
            after.activity_at, old_activity_at,
            "the previous activity was stamped as a new activity"
        );
        assert!(
            after
                .last_work
                .iter()
                .any(|line| line.contains("comment on #7")),
            "the previous last work was lost: {:?}",
            after.last_work
        );
        // The Agent screen's cause row is the current failure, not the old refusal.
        let mut board = crate::board::Board::new(engine);
        for _ in 0..2 {
            if board.screen() == crate::board::Screen::Agent {
                break;
            }
            board.on_key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Enter,
            ));
        }
        assert_eq!(
            board.screen(),
            crate::board::Screen::Agent,
            "the Agent screen did not open"
        );
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("a terminal");
        terminal
            .draw(|frame| board.draw(frame))
            .expect("the frame draws");
        let mut frame = String::new();
        let area = terminal.backend().buffer().area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.push_str(
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .expect("a cell")
                        .symbol(),
                );
            }
            frame.push('\n');
        }
        assert!(
            frame.contains("fixture"),
            "the current start failure is not on the Agent screen:\n{frame}"
        );
        assert!(
            !frame.contains("old refusal"),
            "the previous failure is presented as the current one:\n{frame}"
        );
    }

    #[test]
    fn a_prepare_failure_after_a_warned_run_is_the_current_cause() {
        let root = scratch_root("engine-prepare-provenance");
        assert_start_failure_provenance(&root, true);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_spawn_failure_after_a_warned_run_is_the_current_cause() {
        let root = scratch_root("engine-spawn-provenance");
        assert_start_failure_provenance(&root, false);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_run_does_not_show_the_previous_runs_failure() {
        let root = scratch_root("engine-stale-failure");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let old = root
            .join("state")
            .join("agent-1")
            .join("run-1700000000000-0001");
        fs::create_dir_all(&old).expect("the old run directory");
        fs::write(old.join("last-failure"), b"write refused: HTTP 403 stale")
            .expect("the old failure row");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        assert_eq!(
            engine.board().agents[0].last_failure.as_deref(),
            Some("write refused: HTTP 403 stale"),
            "the old row is not set up"
        );
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("Start");
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        // The fake's start report lands before the engine's session job takes
        // the state lock and installs the new run. Wait for the board state the
        // assertion is about, not only for the spawner, so a loaded host cannot
        // expose the previous row between those two steps.
        wait_until("the new run's board state", || {
            let board = engine.board();
            board.agents[0].last_failure.is_none()
                && matches!(board.agents[0].session, SessionState::Running { .. })
        });
        assert!(
            engine.board().agents[0].last_failure.is_none(),
            "the previous run's failure is still presented beside the new run"
        );
        assert!(
            matches!(
                engine.board().agents[0].session,
                SessionState::Running { .. }
            ),
            "the new run is not the current state"
        );
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_normal_stop_records_its_ending_before_the_handles_and_reopens_clean() {
        let root = scratch_root("engine-normal-stop-reopen");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(
            &root,
            settings.clone(),
            Arc::clone(&clock),
            Arc::clone(&fake),
        );
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the first run's directory");
        fs::create_dir_all(&run_dir).expect("the run directory");
        // The identity handles a real spawner writes. They must be gone only after
        // the ending is durable.
        fs::write(run_dir.join(crate::engine::session::PID_FILE), "424242").expect("pid");
        fs::write(run_dir.join(crate::run::PGID_FILE), "424242").expect("pgid");
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut engine, &clock, Duration::ZERO);
        let terminal = crate::transcript::terminal_record(&run_dir)
            .expect("a normal stop must leave a durable terminal record");
        assert!(
            format!("{terminal:?}").contains("stopped"),
            "the ending is not stopped: {terminal:?}"
        );
        assert!(
            !run_dir.join(crate::engine::session::PID_FILE).exists()
                && !run_dir.join(crate::run::PGID_FILE).exists(),
            "the identity handles were not cleared after the ending"
        );
        drop(engine);
        // The next open finds a finished run, not uncertainty, and Start can make a
        // replacement rather than being blocked forever.
        let mut reopened = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        assert!(
            reopened.board().agents[0].leftover.is_none(),
            "a cleanly stopped run was reported as an unverifiable leftover"
        );
        reopened
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("Start after a clean stop");
        tick(&mut reopened, &clock, Duration::ZERO);
        wait_started(&fake, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_no_work_success_resets_the_failures_and_waits_twenty() {
        let root = scratch_root("engine-no-work");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        // Two quick failures put the streak at two and the wait at 20 s.
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        tick(&mut engine, &clock, Duration::from_secs(10));
        wait_started(&fake, 2);
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        tick(&mut engine, &clock, Duration::from_secs(20));
        wait_started(&fake, 3);
        // The third run succeeded and found no work: the documented 20 s interval,
        // not the 40 s the failure streak would now ask for.
        let run_dir = fake.last_run_dir().expect("the new run's directory");
        fs::create_dir_all(&run_dir).expect("the run directory");
        let mut transcript = crate::transcript::Transcript::open(&run_dir).expect("transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-no-work",
                run::RunOutcome::NoWork,
                Some(0),
                500,
                Some("There are no open issues assigned to me.".to_owned()),
            ))
            .expect("the no-work ending");
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        assert!(
            matches!(
                &engine.board().agents[0].session,
                SessionState::Restarting {
                    last: SessionEnd::NoWork { .. },
                    ..
                }
            ),
            "the no-work success was not kept: {:?}",
            engine.board().agents[0].session
        );
        tick(&mut engine, &clock, Duration::from_secs(19));
        assert_eq!(
            fake.counts().1,
            3,
            "a run started before the documented 20 s no-work wait"
        );
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, 4);
        // The streak reset on the no-work success: the next actual quick failure
        // waits the first 10 s, not 40 s.
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        tick(&mut engine, &clock, Duration::from_secs(9));
        assert_eq!(
            fake.counts().1,
            4,
            "a quick failure after a no-work success used a stale streak"
        );
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, 5);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_quick_failure_waits_ten_then_doubles_to_the_cap() {
        let root = scratch_root("engine-backoff");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let expected = [10u64, 20, 40, 80, 160, 300, 300];
        for (index, delay) in expected.iter().enumerate() {
            fake.finish_current(1);
            tick(&mut engine, &clock, Duration::ZERO);
            let started = fake.counts().1;
            tick(&mut engine, &clock, Duration::from_secs(*delay - 1));
            assert_eq!(
                fake.counts().1,
                started,
                "session {index} started before {delay} s"
            );
            tick(&mut engine, &clock, Duration::from_secs(1));
            wait_started(&fake, started + 1);
        }
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    /// Wave 36 C1: a warned run is not a success, so it must not take the five-second
    /// success loop. It enters the ordinary twenty-second watch interval instead; a
    /// deterministic script then cannot replay the same comment every few seconds.
    /// Wave 36 C3: the engine refuses the same empty key the first-run form
    /// refuses. A UI check is not enough for a command that can be sent directly.
    #[test]
    fn an_empty_key_is_not_saved_for_an_agent() {
        let root = scratch_root("engine-empty-agent-key");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        credentials::remove_secret(&layout.credentials_file(), None, "model_key")
            .expect("the shared key is removed");
        let mut engine = open_test_engine(
            &root,
            settings,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        let no_key = engine.send(Command::SetAgentModel {
            name: "agent-1".to_owned(),
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "demo".to_owned(),
            key: None,
        });
        assert!(
            no_key
                .as_ref()
                .is_err_and(|error| error.contains("API key is empty")),
            "a missing key was saved silently: {no_key:?}"
        );
        let cleared = engine.send(Command::SetAgentModel {
            name: "agent-1".to_owned(),
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "demo".to_owned(),
            key: Some(String::new()),
        });
        assert!(
            cleared
                .as_ref()
                .is_err_and(|error| error.contains("API key is empty")),
            "an explicitly empty key was saved silently: {cleared:?}"
        );
        engine
            .send(Command::SetAgentModel {
                name: "agent-1".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                protocol: "chat-completions".to_owned(),
                model: "demo".to_owned(),
                key: Some("gw-test-key".to_owned()),
            })
            .expect("a present key saves");
        let _ = fs::remove_dir_all(&root);
    }

    /// Wave 36 C4: a save during a live run names both facts — it applies to the
    /// next run and the current run keeps the settings it started with.
    #[test]
    fn a_save_during_a_run_says_the_run_keeps_its_settings() {
        let root = scratch_root("engine-save-during-run");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        engine
            .send(Command::SetAgentModel {
                name: "agent-1".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                protocol: "chat-completions".to_owned(),
                model: "next-model".to_owned(),
                key: Some("fixture-key".to_owned()),
            })
            .expect("save during a run");
        let notice = engine.board().notice.unwrap_or_default();
        assert!(
            notice.contains("this run keeps its settings"),
            "the save did not say the running run keeps its settings: {notice}"
        );
        assert!(
            notice.contains("next run"),
            "the save did not say when it applies: {notice}"
        );
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_model_save_does_not_hide_a_codex_sandbox_prerequisite() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_root("engine-model-save-keeps-hint");
        let install = harness::install_root(&root.join("data"), Program::Codex);
        fs::create_dir_all(install.join("bin")).expect("codex bin");
        fs::create_dir_all(install.join("codex-resources")).expect("codex resources");
        fs::write(install.join("bin/codex"), b"#!/bin/sh\nexit 0\n").expect("codex");
        fs::set_permissions(install.join("bin/codex"), fs::Permissions::from_mode(0o755))
            .expect("codex mode");
        fs::write(
            install.join("codex-resources/bwrap"),
            b"#!/bin/sh\necho 'bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted' >&2\nexit 1\n",
        )
        .expect("bwrap");
        fs::set_permissions(
            install.join("codex-resources/bwrap"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("bwrap mode");
        let record = harness::InstallRecord {
            program: Program::Codex.id().to_owned(),
            version: Program::Codex.pinned_version().to_owned(),
            sha256: "0".repeat(64),
            license: "Apache-2.0".to_owned(),
            entrypoint: "bin/codex".to_owned(),
            resources: Some("codex-resources".to_owned()),
            installed_at: "2026-10-25T00:00:00Z".to_owned(),
        };
        fs::write(
            harness::record_path(&install),
            serde_json::to_vec(&record).expect("record json"),
        )
        .expect("record");
        let mut settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let saved = settings.agent_mut("agent-1").expect("agent");
        saved.set_harness(Program::Codex);
        saved.provider = Some(settings::AgentProvider {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            protocol: "responses".to_owned(),
            model: "lab-codex".to_owned(),
        });
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start probe");
        let hint = engine.board().agents[0]
            .program_error
            .clone()
            .unwrap_or_default();
        assert!(hint.contains("bwrap: loopback"), "{hint}");
        assert!(hint.contains("press f twice"), "{hint}");
        engine
            .send(Command::SetAgentModel {
                name: "agent-1".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                protocol: "responses".to_owned(),
                model: "next-codex".to_owned(),
                key: Some("fixture-key".to_owned()),
            })
            .expect("model save");
        let hint = engine.board().agents[0]
            .program_error
            .clone()
            .unwrap_or_default();
        assert!(
            hint.contains("bwrap: loopback") && hint.contains("press f twice"),
            "a model save hid the still-present sandbox cause: {hint}"
        );
        let mut board = Board::new(engine);
        for _ in 0..2 {
            if board.screen() == crate::board::Screen::Agent {
                break;
            }
            board.on_key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Enter,
            ));
        }
        let backend = ratatui::backend::TestBackend::new(72, 18);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| board.draw(frame))
            .expect("the frame draws");
        let mut frame = String::new();
        let area = terminal.backend().buffer().area;
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.push_str(
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" "),
                );
            }
            frame.push('\n');
        }
        assert!(frame.contains("bwrap: loopback"), "{frame}");
        assert!(frame.contains("press f twice"), "{frame}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sandbox_save_during_a_run_says_the_run_keeps_its_settings() {
        let root = scratch_root("engine-sandbox-save-during-run");
        let mut settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        settings
            .agent_mut("agent-1")
            .expect("agent")
            .set_harness(Program::Builtin);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        engine
            .send(Command::SetAgentProgram {
                name: "agent-1".to_owned(),
                program: "codex".to_owned(),
                protocol: Some("responses".to_owned()),
            })
            .expect("program save during a run");
        engine
            .send(Command::SetAgentCodexSandbox {
                name: "agent-1".to_owned(),
                mode: "full-access".to_owned(),
            })
            .expect("sandbox save during a run");
        let notice = engine.board().notice.unwrap_or_default();
        assert!(
            notice.contains("this run keeps its settings"),
            "the sandbox save did not say the running run keeps its settings: {notice}"
        );
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_warned_run_waits_the_watch_interval_not_the_success_loop() {
        let root = scratch_root("engine-warned-wait");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        write_terminal_record(
            &run_dir,
            run::RunOutcome::Warned,
            "the native program finished without verified success",
        );
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        assert!(
            matches!(
                &engine.board().agents[0].session,
                SessionState::Restarting {
                    last: SessionEnd::Warned { .. },
                    ..
                }
            ),
            "the warned run did not schedule a restart: {:?}",
            engine.board().agents[0].session
        );
        let started = fake.counts().1;
        tick(&mut engine, &clock, Duration::from_secs(4));
        assert_eq!(
            fake.counts().1,
            started,
            "a warned run restarted in the five-second success loop"
        );
        tick(&mut engine, &clock, Duration::from_secs(15));
        assert_eq!(
            fake.counts().1,
            started,
            "a warned run restarted before the twenty-second watch interval"
        );
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, started + 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_long_failure_resets_the_backoff_to_the_visible_idle() {
        let root = scratch_root("engine-long");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        tick(&mut engine, &clock, Duration::from_secs(10));
        wait_started(&fake, 2);
        tick(&mut engine, &clock, Duration::from_secs(60));
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        let started = fake.counts().1;
        tick(&mut engine, &clock, Duration::from_secs(4));
        assert_eq!(fake.counts().1, started, "a long failure did not wait 5 s");
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, started + 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    #[test]
    fn a_session_that_overruns_its_timeout_is_recorded_and_restarted() {
        let root = scratch_root("engine-timeout");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        tick(&mut engine, &clock, DEFAULT_SESSION_TIMEOUT);
        let board = engine.board();
        assert_eq!(board.agents[0].recent.last(), Some(&SessionEnd::TimedOut));
        assert!(matches!(
            &board.agents[0].session,
            SessionState::Restarting {
                last: SessionEnd::TimedOut,
                ..
            }
        ));
        let run_dir = fake.last_run_dir().expect("the timed-out run directory");
        assert_eq!(first_ending(&run_dir), Some(run::FirstEnding::Stop));
        assert!(
            run_dir.join(run::STOPPED_FILE).is_file(),
            "a timed-out run left no stop record"
        );
        tick(&mut engine, &clock, RESTART_DELAY);
        wait_started(&fake, 2);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    fn write_terminal_record(run_dir: &Path, outcome: run::RunOutcome, reason: &str) {
        let run_id = run_id_in(run_dir).expect("the run id");
        let mut transcript =
            crate::transcript::Transcript::open(run_dir).expect("the run transcript");
        transcript
            .append_durable(&crate::run::terminal_record_for_test(
                run_id,
                outcome,
                None,
                10,
                Some(reason.to_owned()),
            ))
            .expect("the terminal record");
    }

    fn terminal_outcomes(run_dir: &Path) -> Vec<(String, Option<String>)> {
        crate::transcript::read(run_dir)
            .expect("the transcript reads")
            .records
            .into_iter()
            .filter_map(|record| match record {
                crate::transcript::Record::RunFinished {
                    outcome, reason, ..
                } => Some((outcome, reason)),
                _ => None,
            })
            .collect()
    }

    fn first_ending(run_dir: &Path) -> Option<run::FirstEnding> {
        run::recorded_first_ending(run_dir).expect("the first ending reads")
    }

    #[test]
    fn a_natural_finish_persists_terminal_as_the_first_ending() {
        let root = scratch_root("engine-natural-fact");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        assert_eq!(
            first_ending(&run_dir),
            Some(run::FirstEnding::Terminal),
            "a natural finish was recorded as an engine stop"
        );
        assert!(!run_dir.join(run::STOPPED_FILE).is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stop_after_the_runs_ending_keeps_the_one_ending_and_writes_no_stop_record() {
        let root = scratch_root("engine-finish-then-stop");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        write_terminal_record(
            &run_dir,
            run::RunOutcome::Succeeded,
            "the run finished before the stop",
        );
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        assert_eq!(first_ending(&run_dir), Some(run::FirstEnding::Terminal));
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let board = engine.board();
        assert_eq!(board.agents[0].recent.last(), Some(&SessionEnd::Succeeded));
        assert!(
            !run_dir.join(run::STOPPED_FILE).is_file(),
            "a late stop wrote a false stop record"
        );
        assert_eq!(
            terminal_outcomes(&run_dir),
            vec![(
                "succeeded".to_owned(),
                Some("the run finished before the stop".to_owned())
            )],
            "the stop changed the terminal record"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_stop_after_the_childs_ending_keeps_the_first_stops_stopped() {
        let root = scratch_root("engine-stop-then-finish");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("first stop");
        assert_eq!(first_ending(&run_dir), Some(run::FirstEnding::Stop));
        // The child managed to write an ending during the first stop's grace.
        write_terminal_record(&run_dir, run::RunOutcome::Succeeded, "a post-signal ending");
        // A second Stop must not re-sample the ordering fact and hand the run to the
        // child's post-signal ending.
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("second stop");
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let board = engine.board();
        assert_eq!(board.agents[0].recent.last(), Some(&SessionEnd::Stopped));
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        let endings = terminal_outcomes(&run_dir);
        assert_eq!(
            endings.len(),
            1,
            "the run kept two terminal records: {endings:?}"
        );
        assert_eq!(endings[0].0, "stopped");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_crash_after_a_persisted_stop_reopens_as_stopped() {
        let child_env = "GEWU_CRASH_REOPEN_CHILD";
        if std::env::var_os(child_env).is_some() {
            let root =
                PathBuf::from(std::env::var("GEWU_CRASH_REOPEN_ROOT").expect("the crash root"));
            let settings = Settings::load(&settings::path_in(&root)).expect("the crash settings");
            let clock = Arc::new(FakeClock::new());
            let fake = Arc::new(FakeSpawner::new());
            let mut engine = open_test_engine(&root, settings, clock, Arc::clone(&fake));
            tick(&mut engine, &FakeClock::new(), Duration::ZERO);
            wait_started(&fake, 1);
            let run_dir = fake.last_run_dir().expect("the crash run directory");
            engine
                .send(Command::Stop {
                    name: "agent-1".to_owned(),
                })
                .expect("the first stop");
            write_terminal_record(
                &run_dir,
                run::RunOutcome::Succeeded,
                "the child finished during the first stop's grace",
            );
            assert_eq!(
                first_ending(&run_dir),
                Some(run::FirstEnding::Stop),
                "the first stop was not durable before the crash"
            );
            fs::write(
                root.join("crash-run-dir.txt"),
                run_dir.display().to_string(),
            )
            .expect("the crash run path");
            // Hard crash: no Drop, no shutdown, no settle.
            std::process::abort();
        }

        let root = scratch_root("engine-crash-reopen");
        write_ready(&root, &[("agent-1", Desired::Started)]);
        let exe = std::env::current_exe().expect("the test executable");
        let status = std::process::Command::new(exe)
            .arg("--exact")
            .arg("engine::tests::a_crash_after_a_persisted_stop_reopens_as_stopped")
            .arg("--nocapture")
            .env(child_env, "1")
            .env("GEWU_CRASH_REOPEN_ROOT", &root)
            .status()
            .expect("the crashing child runs");
        assert!(!status.success(), "the child did not crash");
        let run_dir = PathBuf::from(
            fs::read_to_string(root.join("crash-run-dir.txt")).expect("the crash run path"),
        );
        let settings = Settings::load(&settings::path_in(&root)).expect("the reopen settings");
        let mut reopened = open_test_engine(
            &root,
            settings,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
        );
        tick(&mut reopened, &FakeClock::new(), Duration::ZERO);
        assert_eq!(
            first_ending(&run_dir),
            Some(run::FirstEnding::Stop),
            "the reopen lost the first-ending fact"
        );
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        let endings = terminal_outcomes(&run_dir);
        assert_eq!(endings.len(), 1, "the reopen kept two endings: {endings:?}");
        assert_eq!(endings[0].0, "stopped");
        assert!(reopened.board().agents[0].leftover.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_explicit_stop_fact_failure_marks_uncertainty_and_does_not_signal() {
        let root = scratch_root("engine-stop-uncertain");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        let pid = fake.live()[0];
        fs::write(
            run_dir.join(run::FIRST_ENDING_FILE),
            r#"{"ts":"2026-10-24T17:00:00.000Z","run_id":"run-9999999999999-9999","ending":"stop"}"#,
        )
        .expect("the foreign fact");
        let error = engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect_err("a stop without a durable fact must fail");
        assert!(error.contains("first ending"), "{error}");
        assert!(fake.is_live(pid), "the stop signal was sent without a fact");
        assert!(run_dir.join(run::ENDING_UNCERTAIN_FILE).is_file());
        drop(engine);
        assert!(!fake.is_live(pid), "shutdown did not stop the child");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deadline_fact_failure_marks_uncertainty_and_does_not_signal() {
        let root = scratch_root("engine-deadline-uncertain");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        let pid = fake.live()[0];
        fs::write(
            run_dir.join(run::FIRST_ENDING_FILE),
            r#"{"ts":"2026-10-24T17:00:00.000Z","run_id":"run-9999999999999-9999","ending":"stop"}"#,
        )
        .expect("the foreign fact");
        tick(&mut engine, &clock, DEFAULT_SESSION_TIMEOUT);
        assert!(
            fake.is_live(pid),
            "the deadline signal was sent without a fact"
        );
        assert!(run_dir.join(run::ENDING_UNCERTAIN_FILE).is_file());
        drop(engine);
        assert!(!fake.is_live(pid), "shutdown did not stop the child");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_shutdown_fact_write_failure_records_uncertainty_and_keeps_handles() {
        let root = scratch_root("engine-shutdown-uncertain");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        // An obstructed fact path makes every fact write fail while the rest of
        // the run directory remains writable for the uncertainty evidence.
        fs::create_dir(run_dir.join(run::FIRST_ENDING_FILE)).expect("the obstructed fact");
        engine.shutdown(Duration::from_millis(200));
        assert!(run_dir.join(run::ENDING_UNCERTAIN_FILE).is_file());
        assert!(
            !run_dir.join(run::STOPPED_FILE).is_file(),
            "a clean stop was claimed without a fact"
        );
        let error = run::recover_ending(&run_dir, Duration::from_millis(20))
            .expect_err("the uncertain run must not recover cleanly");
        assert!(error.contains("uncertain"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_shutdown_after_the_childs_ending_keeps_the_first_stops_stopped() {
        let root = scratch_root("engine-stop-then-shutdown");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("first stop");
        write_terminal_record(
            &run_dir,
            run::RunOutcome::Succeeded,
            "a post-stop ending before shutdown",
        );
        engine.shutdown(Duration::from_millis(200));
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        let endings = terminal_outcomes(&run_dir);
        assert_eq!(
            endings.len(),
            1,
            "shutdown kept two terminal records: {endings:?}"
        );
        assert_eq!(endings[0].0, "stopped");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deadline_after_the_runs_ending_keeps_the_one_succeeded_outcome() {
        let root = scratch_root("engine-finish-then-deadline");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        write_terminal_record(
            &run_dir,
            run::RunOutcome::Succeeded,
            "the run finished before the deadline",
        );
        tick(&mut engine, &clock, DEFAULT_SESSION_TIMEOUT);
        assert_eq!(first_ending(&run_dir), Some(run::FirstEnding::Terminal));
        let board = engine.board();
        assert_eq!(board.agents[0].recent.last(), Some(&SessionEnd::Succeeded));
        assert!(
            !run_dir.join(run::STOPPED_FILE).is_file(),
            "a deadline after the ending wrote a false stop record"
        );
        assert_eq!(terminal_outcomes(&run_dir).len(), 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deadline_with_a_failed_tool_stays_timed_out_and_names_the_warning() {
        let root = scratch_root("engine-deadline-tool-warning");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        run::write_native_progress(
            &run_dir,
            "running",
            "",
            false,
            Some("shell: the native tool exited with code 1"),
        )
        .expect("the native progress record");
        tick(&mut engine, &clock, DEFAULT_SESSION_TIMEOUT);
        let board = engine.board();
        match board.agents[0].recent.last() {
            Some(SessionEnd::TimedOut) => {}
            other => panic!("a failed tool changed the deadline ending: {other:?}"),
        }
        let stop: run::RunStopped = serde_json::from_str(
            &fs::read_to_string(run_dir.join(run::STOPPED_FILE)).expect("stop record"),
        )
        .expect("stop record JSON");
        assert!(
            stop.reason
                .contains("tool warning: shell: the native tool exited with code 1"),
            "{}",
            stop.reason
        );
        let terminal = crate::transcript::terminal_record(&run_dir).expect("terminal record");
        match terminal {
            crate::transcript::Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            } => {
                assert_eq!(outcome, "timed_out");
                assert!(reason.contains("tool warning"), "{reason}");
            }
            other => panic!("unexpected terminal record: {other:?}"),
        }
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deadline_keeps_the_native_failure_cause_instead_of_only_timing_out() {
        let root = scratch_root("engine-deadline-cause");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let run_dir = fake.last_run_dir().expect("the run directory");
        run::write_native_progress(
            &run_dir,
            "retrying",
            "the native provider reported: synthetic status 500",
            false,
            None,
        )
        .expect("the native progress record");
        tick(&mut engine, &clock, DEFAULT_SESSION_TIMEOUT);
        let board = engine.board();
        match board.agents[0].recent.last() {
            Some(SessionEnd::Failed { reason }) => {
                assert!(reason.contains("500"), "the cause was lost: {reason}");
            }
            other => panic!("a failing harness was recorded only as {other:?}"),
        }
        let stop: run::RunStopped = serde_json::from_str(
            &fs::read_to_string(run_dir.join(run::STOPPED_FILE)).expect("stop record"),
        )
        .expect("stop record JSON");
        assert!(stop.reason.contains("500"), "{}", stop.reason);
        let terminal = crate::transcript::terminal_record(&run_dir).expect("terminal record");
        assert!(
            matches!(
                &terminal,
                crate::transcript::Record::RunFinished { outcome, .. } if outcome == "failed"
            ),
            "the terminal record lost the failure: {terminal:?}"
        );
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stop_during_a_session_ends_it_and_nothing_restarts() {
        let root = scratch_root("engine-stop-live");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut engine, &clock, Duration::from_secs(60));
        assert_eq!(fake.counts().1, 1, "a stopped agent restarted");
        assert!(matches!(
            &engine.board().agents[0].session,
            SessionState::Idle
        ));
    }

    #[test]
    fn stop_during_a_backoff_keeps_the_agent_idle() {
        let root = scratch_root("engine-stop-backoff");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        fake.finish_current(1);
        tick(&mut engine, &clock, Duration::ZERO);
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut engine, &clock, Duration::from_secs(600));
        assert_eq!(fake.counts().1, 1, "a stopped back-off started a session");
    }

    #[test]
    fn blocked_names_the_missing_thing_and_starts_when_it_is_gone() {
        let root = scratch_root("engine-blocked");
        let mut settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        settings.endpoint.url.clear();
        settings
            .save(&settings::path_in(
                Layout::rooted_at(root.clone())
                    .expect("the test layout is absolute")
                    .root(),
            ))
            .expect("settings");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let board = engine.board();
        match &board.agents[0].session {
            SessionState::Blocked { reason } => assert_eq!(reason, "needs a model"),
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(fake.counts().1, 0, "a blocked agent started a session");
        engine
            .send(Command::SetEndpoint {
                url: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
                model: "lab-chat-1".to_owned(),
                key: None,
            })
            .expect("endpoint");
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    #[test]
    fn the_endpoint_test_reuses_the_model_probe_in_the_background() {
        let root = scratch_root("engine-endpoint-test");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let stub = crate::test_stub::Stub::start(vec![crate::test_stub::StubReply::status(
            200,
            serde_json::json!({
                "model": "lab-chat-1",
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "ok"}
                }]
            })
            .to_string(),
        )]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, clock, fake);
        engine
            .send(Command::SetEndpoint {
                url: stub.url(),
                model: "lab-chat-1".to_owned(),
                key: Some("gw-test-key".to_owned()),
            })
            .expect("endpoint");
        engine.send(Command::TestEndpoint).expect("test");
        wait_until("the endpoint test", || {
            let board = engine.board();
            match board.endpoint.test {
                EndpointTest::Passed { answer } => {
                    assert_eq!(answer, "the model answered");
                    true
                }
                EndpointTest::Failed { reason } => panic!("the probe failed: {reason}"),
                _ => false,
            }
        });
    }

    #[test]
    fn c7_a_silent_package_install_still_shows_an_elapsed_time() {
        let root = scratch_root("engine-install-elapsed");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, clock, fake);
        {
            let mut inner = lock_inner(&engine.inner);
            inner.program_states.insert(
                "dsh".to_owned(),
                ProgramState {
                    installing: Some(None),
                    phase: Some(harness::InstallPhase::InstallingPackages),
                    started: Some(Instant::now() - Duration::from_secs(12)),
                    ..ProgramState::default()
                },
            );
        }
        let board = engine.board();
        let dsh = board
            .programs
            .iter()
            .find(|program| program.id == "dsh")
            .expect("the DSH card");
        let phase = dsh.installing_phase.as_deref().expect("an install phase");
        assert!(phase.starts_with("installing packages"), "{phase}");
        assert!(phase.contains("12s") || phase.contains("13s"), "{phase}");
    }

    #[test]
    fn c3_the_per_agent_test_uses_that_agents_own_connection() {
        let root = scratch_root("engine-agent-test");
        let mut settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let stub = crate::test_stub::Stub::start(vec![crate::test_stub::StubReply::status(
            200,
            serde_json::json!({
                "model": "lab-chat-1",
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "ok"}
                }]
            })
            .to_string(),
        )]);
        let base = stub.url().trim_end_matches("/chat/completions").to_owned();
        settings.agent_mut("agent-1").expect("agent").provider = Some(settings::AgentProvider {
            base_url: base,
            protocol: "chat-completions".to_owned(),
            model: "lab-chat-1".to_owned(),
        });
        settings
            .save(&settings::path_in(
                Layout::rooted_at(root.clone())
                    .expect("the test layout is absolute")
                    .root(),
            ))
            .expect("settings");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, clock, fake);
        engine
            .send(Command::TestAgentModel {
                name: "agent-1".to_owned(),
            })
            .expect("test");
        wait_until("the per-agent model test", || {
            match &engine.board().agents[0].model_test {
                EndpointTest::Passed { answer } => {
                    assert_eq!(answer, "the model answered");
                    true
                }
                EndpointTest::Failed { reason } => panic!("the agent test failed: {reason}"),
                _ => false,
            }
        });
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/chat/completions");
        assert_eq!(
            requests[0].header("authorization").as_deref(),
            Some("Bearer gw-test-key")
        );
    }

    #[test]
    fn an_empty_instruction_blocks_and_writing_one_releases_it() {
        let root = scratch_root("engine-instruction");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
        let paths = layout.agent("agent-1").expect("paths");
        fs::remove_file(paths.instruction()).expect("the instruction is removed");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        match &engine.board().agents[0].session {
            SessionState::Blocked { reason } => {
                assert!(reason.contains("instruction"), "{reason}")
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(fake.counts().1, 0);
        fs::write(paths.instruction(), "do the work now\n").expect("instruction");
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, 1);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
    }

    #[test]
    fn a_started_agent_resumes_after_reopening() {
        let root = scratch_root("engine-resume");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut first = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        first
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut first, &clock, Duration::ZERO);
        drop(first);
        let reopened_settings =
            Settings::load(&settings::path_in(root.as_path())).expect("settings");
        assert_eq!(
            reopened_settings.agent("agent-1").expect("agent").desired(),
            Desired::Stopped
        );
        // A person starts it again, closes the program, and opens it: the saved choice
        // is started, so a session runs without another Start.
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut first = open_test_engine(
            &root,
            reopened_settings,
            Arc::clone(&clock),
            Arc::clone(&fake),
        );
        first
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        tick(&mut first, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        fake.finish_current(0);
        tick(&mut first, &clock, Duration::ZERO);
        drop(first);
        let reopened = Settings::load(&settings::path_in(root.as_path())).expect("settings");
        assert_eq!(
            reopened.agent("agent-1").expect("agent").desired(),
            Desired::Started
        );
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut again = open_test_engine(&root, reopened, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut again, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        again
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        tick(&mut again, &clock, Duration::ZERO);
    }
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    /// A starter that spawns a real sleeping child, reports its pid, and then blocks
    /// until the test releases it. It models a slow readiness wait that a shutdown
    /// must be able to interrupt.
    #[derive(Debug)]
    struct BlockingForgejoStarter {
        entered: Mutex<Option<std::sync::mpsc::Sender<u32>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        finished: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        log: PathBuf,
        pid_file: PathBuf,
    }

    impl BlockingForgejoStarter {
        fn new(
            root: &Path,
        ) -> (
            Arc<Self>,
            std::sync::mpsc::Receiver<u32>,
            std::sync::mpsc::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            (
                Arc::new(Self {
                    entered: Mutex::new(Some(entered_tx)),
                    release: Mutex::new(release_rx),
                    finished: Mutex::new(Some(finished_tx)),
                    log: root.join("fake-forgejo.log"),
                    pid_file: root.join("fake-forgejo.pid"),
                }),
                entered_rx,
                release_tx,
                finished_rx,
            )
        }

        fn finished(&self) {
            if let Some(tx) = self
                .finished
                .lock()
                .expect("the fake is not poisoned")
                .take()
            {
                let _ = tx.send(());
            }
        }
    }

    impl ForgejoStarter for BlockingForgejoStarter {
        fn start(
            &self,
            _binary: &Path,
            _prepared: PreparedInstance,
            _ready_timeout: Duration,
            on_spawn: &mut dyn FnMut(u32),
            cancelled: &dyn Fn() -> bool,
        ) -> Result<RunningForgejo, String> {
            let child = std::process::Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("the fake server starts");
            let pid = child.id();
            on_spawn(pid);
            if let Some(tx) = self
                .entered
                .lock()
                .expect("the fake is not poisoned")
                .take()
            {
                let _ = tx.send(pid);
            }
            let _ = self
                .release
                .lock()
                .expect("the fake is not poisoned")
                .recv();
            let mut server = bundled_forgejo::test_server(
                child,
                "http://127.0.0.1:3000".to_owned(),
                self.log.clone(),
                self.pid_file.clone(),
            );
            if cancelled() {
                server.stop(Duration::from_secs(2));
                self.finished();
                return Err("the start was called off".to_owned());
            }
            self.finished();
            Ok(server)
        }
    }

    /// Whether a pid exists and is not a zombie: a zombie still answers `kill -0`,
    /// but it is no longer a running child.
    fn process_running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| text.split_whitespace().nth(2).map(|state| state != "Z"))
            .unwrap_or(false)
    }

    #[test]
    fn shutdown_ends_a_forgejo_token_helper_that_is_still_running() {
        let root = scratch_root("engine-token-helper-stop");
        let settings = write_ready(&root, &[]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        bundled_forgejo::InstancePaths::under(&root)
            .ensure()
            .expect("the instance paths");

        let pid_file = root.join("token-helper.pid");
        let helper = root.join("fake-forgejo-token.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/bash\ntrap '' TERM HUP\nprintf '%s' \"$$\" > '{}'\nwhile :; do read -t 1 _ || true; done\n",
                pid_file.display()
            ),
        )
        .expect("the fake token helper is written");
        crate::mode::set_mode(&helper, 0o755).expect("the fake helper is executable");

        let token_inner = Arc::clone(&engine.inner);
        let cancel_inner = Arc::clone(&token_inner);
        let (job_tx, job_rx) = std::sync::mpsc::channel();
        let job = std::thread::spawn(move || {
            let result = ensure_admin_token(&token_inner, &helper, &root, "tester", &move || {
                lock_inner(&cancel_inner).stopping
            });
            let _ = job_tx.send(result);
        });
        wait_until("the token helper's pid record", || pid_file.is_file());
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the token helper wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(process_alive(pid), "the token helper did not start");

        // A shutdown that waited for this helper (which traps TERM) would never
        // return; the liveness bound fails the test instead of hanging it.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let shutdown_thread = std::thread::spawn(move || {
            engine.shutdown(Duration::from_millis(200));
            let _ = done_tx.send(engine);
        });
        let engine = recv_within("shutdown with a token helper running", &done_rx);
        shutdown_thread.join().expect("the shutdown thread ends");
        let result = recv_within("the token job's end", &job_rx);
        job.join().expect("the token job ends");
        assert!(result.is_err(), "the helper was not called off");
        assert!(
            !process_alive(pid),
            "the token helper {pid} was left running or unreaped"
        );
        assert!(
            engine.live_pids().is_empty(),
            "the engine still records {:?}",
            engine.live_pids()
        );
    }

    #[test]
    fn shutdown_stops_a_forgejo_whose_start_is_still_in_flight() {
        let root = scratch_root("engine-quit-forgejo-start");
        let (starter, entered, release, finished) = BlockingForgejoStarter::new(&root);
        let mut options = test_options(&root);
        options.forgejo_ready = false;
        options.auto_start_forgejo = false;
        options.stop_grace = Duration::from_secs(2);
        options.forgejo_starter = Some(starter);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_for_test(options, clock, fake, Settings::default())
            .expect("the test engine opens");
        engine.send(Command::StartForgejo).expect("start Forgejo");
        engine.tick();
        let pid = recv_within("the fake server's child", &entered);
        assert!(process_alive(pid), "the fake child did not start");

        // If shutdown waited for the blocked start, the test would hang; the
        // liveness receive fails it at the bound instead.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let shutdown_thread = std::thread::spawn(move || {
            engine.shutdown(Duration::from_millis(250));
            let _ = done_tx.send(engine);
        });
        let mut engine = recv_within("shutdown during an in-flight Forgejo start", &done_rx);
        shutdown_thread.join().expect("the shutdown thread ends");
        // The child was signalled during the block. It may be a zombie until the job
        // that owns it is released and reaps it, but it is no longer running.
        assert!(
            !process_running(pid),
            "pid {pid} is still running after shutdown"
        );
        assert!(
            engine.live_pids().is_empty(),
            "the engine still records {:?}",
            engine.live_pids()
        );

        // Releasing after shutdown must finish the job and reap the child, not keep
        // the server for a later start.
        release.send(()).expect("the fake is released");
        recv_within("the start job's end after shutdown", &finished);
        wait_until("the released start job's child to be reaped", || {
            !process_alive(pid)
        });
        assert!(!process_alive(pid), "pid {pid} was kept after release");
        assert!(
            engine.live_pids().is_empty(),
            "the engine kept a child: {:?}",
            engine.live_pids()
        );
        assert!(matches!(
            engine.board().forgejo.state,
            ForgejoState::Stopped
        ));
    }

    #[test]
    fn a_persisted_nonstandard_login_reaches_the_floor_board() {
        for count in [4, 7, 12] {
            let root = scratch_root(&format!("engine-recorded-login-{count}"));
            let names: Vec<String> = (1..=count)
                .map(|number| format!("agent-{number}"))
                .collect();
            let agents: Vec<(&str, Desired)> = names
                .iter()
                .map(|name| (name.as_str(), Desired::Stopped))
                .collect();
            let settings = write_ready(&root, &agents);
            let layout = Layout::rooted_at(root.clone()).expect("layout");
            credentials::record_agent_identity(
                &layout.credentials_file(),
                "agent-2",
                Some(&credentials::Secret::new("synthetic-agent-token")),
                "alice",
                "alice@example.invalid",
                "managed",
                None,
            )
            .expect("recorded identity");
            credentials::record_agent_identity_rights(
                &layout.credentials_file(),
                "agent-2",
                crate::identity::TOOL_RIGHTS_VERSION,
            )
            .expect("recorded rights");
            assert_eq!(
                credentials::recorded_login(&layout.credentials_file(), "agent-2")
                    .expect("read login"),
                Some("alice".to_owned())
            );
            let clock = Arc::new(FakeClock::new());
            let fake = Arc::new(FakeSpawner::new());
            let mut options = test_options(&root);
            options.forgejo_ready = false;
            let mut engine =
                open_for_test(options, clock, fake, settings).expect("the test engine opens");
            assert_eq!(
                engine.board().agents[1].login,
                "alice",
                "the persisted login did not reach AgentRuntime"
            );
            let mut board = crate::board::Board::new(engine);
            for _ in 0..2 {
                if board.screen() == crate::board::Screen::Home {
                    break;
                }
                board.on_key(ratatui::crossterm::event::KeyEvent::from(
                    ratatui::crossterm::event::KeyCode::Enter,
                ));
            }
            assert_eq!(board.screen(), crate::board::Screen::Home);
            let backend = ratatui::backend::TestBackend::new(72, 18);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| board.draw(frame))
                .expect("the floor draws");
            let area = terminal.backend().buffer().area;
            let mut frame = String::new();
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    frame.push_str(
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .expect("cell")
                            .symbol(),
                    );
                }
                frame.push('\n');
            }
            let row = board
                .layout()
                .listed
                .iter()
                .find(|slot| slot.agent == 1)
                .map(|slot| slot.rect)
                .expect("the persisted login's row");
            let mut own = String::new();
            for x in row.x..row.right() {
                own.push_str(
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, row.y))
                        .expect("cell")
                        .symbol(),
                );
            }
            assert!(
                own.contains("2:alice") && frame.contains("N=agent-N"),
                "{count}: the persisted login was not mapped to its row: {own:?}\n{frame}"
            );
            assert!(
                !frame.contains("login=gewu-<name>"),
                "{count}: a generated-login legend claimed the persisted login"
            );
            let _ = fs::remove_dir_all(&root);
        }
    }

    /// A test spawner that creates a real child, writes the real pid/group records
    /// and a typed `RunStarted`, reports the pid, then waits for the test before
    /// returning. It uses the production `SessionSpawner` boundary, so the engine's
    /// called-off/removal/shutdown code sees a run that actually started.
    struct RealChildSpawner {
        report: std::sync::Mutex<Option<std::sync::mpsc::Sender<u32>>>,
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    struct RealChild {
        child: std::process::Child,
        pid: u32,
        exited: Option<std::process::ExitStatus>,
    }

    impl crate::engine::session::RunningSession for RealChild {
        fn pid(&self) -> u32 {
            self.pid
        }
        fn try_status(&mut self) -> Option<std::process::ExitStatus> {
            if self.exited.is_none() {
                self.exited = self.child.try_wait().ok().flatten();
            }
            self.exited
        }
        fn signal(&mut self, force: bool) {
            let signal = if force {
                nix::sys::signal::Signal::SIGKILL
            } else {
                nix::sys::signal::Signal::SIGTERM
            };
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pid as i32), signal);
        }
    }

    impl Drop for RealChild {
        fn drop(&mut self) {
            if self.exited.is_none() {
                crate::engine::session::RunningSession::signal(self, true);
                let _ = self.child.wait();
            }
        }
    }

    impl crate::engine::session::SessionSpawner for RealChildSpawner {
        fn prepare(
            &self,
            request: &crate::engine::session::SessionRequest,
        ) -> Result<crate::engine::session::PreparedSession, String> {
            let guard =
                crate::run::Owner::prepare(&request.run_dir, &request.run_id, &request.workspace)?;
            Ok(crate::engine::session::PreparedSession {
                run_dir: request.run_dir.clone(),
                workspace: request.workspace.clone(),
                guard,
            })
        }
        fn spawn_with(
            &self,
            run_dir: &std::path::Path,
            workspace: &std::path::Path,
            on_spawn: &mut dyn FnMut(u32),
        ) -> Result<Box<dyn crate::engine::session::RunningSession>, String> {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|error| format!("the real child cannot start: {error}"))?;
            let pid = child.id();
            crate::mode::write_private(
                run_dir.join(crate::engine::session::PID_FILE),
                pid.to_string().as_bytes(),
            )
            .map_err(|error| format!("the pid cannot be written: {error}"))?;
            crate::mode::write_private(
                run_dir.join(crate::run::PGID_FILE),
                pid.to_string().as_bytes(),
            )
            .map_err(|error| format!("the group cannot be written: {error}"))?;
            let mut transcript = crate::transcript::Transcript::open(run_dir)
                .map_err(|error| format!("the transcript cannot open: {error}"))?;
            transcript
                .append(&crate::transcript::Record::run_started(
                    run_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("run"),
                    "agent-1",
                    pid,
                    &workspace.display().to_string(),
                ))
                .map_err(|error| format!("the start record cannot be written: {error}"))?;
            on_spawn(pid);
            if let Some(report) = self.report.lock().expect("report").take() {
                let _ = report.send(pid);
            }
            if let Some(release) = self.release.lock().expect("release").take() {
                // The test removes the agent before releasing this boundary. Under
                // a loaded parallel suite that setup can take far longer than a
                // short deadline; the shared liveness bound keeps the spawn in
                // flight until the removal has happened.
                crate::test_wait::recv_within("the test's release of the spawn boundary", &release);
            }
            Ok(Box::new(RealChild {
                child,
                pid,
                exited: None,
            }))
        }
    }

    fn real_child_spawner() -> (
        Arc<RealChildSpawner>,
        std::sync::mpsc::Receiver<u32>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (report_tx, report_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(RealChildSpawner {
                report: std::sync::Mutex::new(Some(report_tx)),
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
            report_rx,
            release_tx,
        )
    }

    /// Whether the whole observable completion of a real-child spawn is visible.
    ///
    /// The runtime contract writes the durable typed ending **before** it discards
    /// the verified identity handles. A test that stops at the ending can therefore
    /// observe the intentional intermediate state. Wait for every fact the final
    /// assertions require: the typed start, the ending, no pid/group records, no
    /// recorded group and no live group member.
    fn real_child_completion_visible(run_dir: &Path, pid: u32) -> bool {
        let typed_start = crate::transcript::read(run_dir)
            .map(|file| {
                file.records.iter().any(|record| matches!(
                    record,
                    crate::transcript::Record::RunStarted { pid: started, .. } if *started == pid
                ))
            })
            .unwrap_or(false);
        typed_start
            && crate::transcript::terminal_record(run_dir).is_some()
            && !run_dir.join(crate::engine::session::PID_FILE).exists()
            && !run_dir.join(crate::run::PGID_FILE).exists()
            && crate::run::recorded_group(run_dir).is_none()
            && !process_group_has_members(pid)
    }

    fn assert_real_child_settled(run_dir: &Path, pid: u32) {
        assert!(
            crate::transcript::read(run_dir)
                .map(|file| file.records.iter().any(|record| matches!(
                    record,
                    crate::transcript::Record::RunStarted { pid: started, .. } if *started == pid
                )))
                .unwrap_or(false),
            "the cancelled spawn had no durable typed RunStarted"
        );
        assert!(
            crate::transcript::terminal_record(run_dir).is_some(),
            "the cancelled spawn had no durable ending"
        );
        assert!(
            !run_dir.join(crate::engine::session::PID_FILE).exists()
                && !run_dir.join(crate::run::PGID_FILE).exists(),
            "the cancelled spawn kept its identity handles"
        );
        let members = crate::run::recorded_group(run_dir);
        assert!(members.is_none(), "the handles remained: {members:?}");
        assert!(
            std::fs::read_dir("/proc").is_ok(),
            "the process table is unreadable"
        );
    }

    #[test]
    fn a_real_child_cancelled_during_spawn_has_a_durable_start_and_settles() {
        let root = scratch_root("engine-real-child-stop-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let (spawner, report, release) = real_child_spawner();
        let mut engine = open_for_test(
            test_options(&root),
            Arc::clone(&clock) as SharedClock,
            spawner,
            settings,
        )
        .expect("engine opens");
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the real child at its spawn boundary", &report);
        let run_dir = newest_run_for_test(&root, "agent-1");
        assert_real_child_started(&run_dir, pid);
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("Stop");
        let _ = release.send(());
        wait_until("the real child's whole completion", || {
            tick(&mut engine, &clock, Duration::ZERO);
            real_child_completion_visible(&run_dir, pid)
        });
        assert_real_child_settled(&run_dir, pid);
        assert!(
            !process_group_has_members(pid),
            "the cancelled real child's group survived"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_real_child_shutdown_during_spawn_has_a_durable_start_and_settles() {
        let root = scratch_root("engine-real-child-shutdown-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let (spawner, report, release) = real_child_spawner();
        let mut engine = open_for_test(
            test_options(&root),
            Arc::clone(&clock) as SharedClock,
            spawner,
            settings,
        )
        .expect("engine opens");
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the real child at its spawn boundary", &report);
        let run_dir = newest_run_for_test(&root, "agent-1");
        assert_real_child_started(&run_dir, pid);
        engine.shutdown(Duration::from_millis(200));
        let _ = release.send(());
        wait_until("the real child's whole completion", || {
            real_child_completion_visible(&run_dir, pid)
        });
        assert_real_child_settled(&run_dir, pid);
        assert!(!process_group_has_members(pid));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_real_child_removed_during_spawn_has_a_durable_start_and_settles() {
        let root = scratch_root("engine-real-child-removed-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let (spawner, report, release) = real_child_spawner();
        let mut engine = open_for_test(
            test_options(&root),
            Arc::clone(&clock) as SharedClock,
            spawner,
            settings,
        )
        .expect("engine opens");
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the real child at its spawn boundary", &report);
        let run_dir = newest_run_for_test(&root, "agent-1");
        assert_real_child_started(&run_dir, pid);
        {
            let mut inner = lock_inner(&engine.inner);
            assert!(inner.settings.remove_agent("agent-1"));
            inner.agents.clear();
        }
        let _ = release.send(());
        wait_until("the real child's whole completion", || {
            tick(&mut engine, &clock, Duration::ZERO);
            real_child_completion_visible(&run_dir, pid)
        });
        assert_real_child_settled(&run_dir, pid);
        assert!(!process_group_has_members(pid));
        let _ = fs::remove_dir_all(&root);
    }

    fn newest_run_for_test(root: &Path, agent: &str) -> PathBuf {
        let mut runs = crate::run::runs_of_agent(&root.join("state"), agent);
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        runs.pop().expect("a run directory").dir
    }

    fn assert_real_child_started(run_dir: &Path, pid: u32) {
        assert!(
            crate::transcript::read(run_dir)
                .map(|file| file.records.iter().any(|record| matches!(
                    record,
                    crate::transcript::Record::RunStarted { pid: started, .. } if *started == pid
                )))
                .unwrap_or(false),
            "the real spawn had no durable typed RunStarted"
        );
        assert!(
            run_dir.join(crate::engine::session::PID_FILE).is_file()
                && run_dir.join(crate::run::PGID_FILE).is_file(),
            "the real spawn had no identity handles"
        );
    }

    fn process_group_has_members(pgid: u32) -> bool {
        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|text| text.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // The group is the third field after the parenthesised command.
            let Some(rest) = stat.rsplit(')').next() else {
                continue;
            };
            if rest.split_whitespace().nth(2) == Some(pgid.to_string().as_str()) {
                return true;
            }
        }
        false
    }

    #[test]
    fn a_stop_during_prepare_records_a_no_child_ending() {
        let root = scratch_root("engine-stop-before-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (prepared_rx, release) = fake.block_next_prepare();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        recv_within("the fake session's prepared run directory", &prepared_rx);
        let run_dir = fake.last_run_dir().expect("the run directory");
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop before spawn");
        let _ = release.send(());
        wait_until("the run's durable ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        let terminal = crate::transcript::terminal_record(&run_dir)
            .expect("a prepared run cancelled before spawn has no ending");
        match terminal {
            crate::transcript::Record::RunFinished { outcome, .. } => {
                assert_eq!(outcome, "stopped")
            }
            _ => panic!("not a terminal"),
        }
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        assert!(fake.live().is_empty(), "a child was spawned after the stop");
    }

    #[test]
    fn a_shutdown_during_prepare_records_a_no_child_ending() {
        let root = scratch_root("engine-shutdown-before-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (prepared_rx, release) = fake.block_next_prepare();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        recv_within("the fake session's prepared run directory", &prepared_rx);
        let run_dir = fake.last_run_dir().expect("the run directory");
        std::thread::scope(|scope| {
            let shutdown = scope.spawn(|| engine.shutdown(Duration::ZERO));
            let _ = release.send(());
            shutdown.join().expect("shutdown");
        });
        wait_until("the run's durable ending", || {
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        let terminal = crate::transcript::terminal_record(&run_dir)
            .expect("a prepared run shut down before spawn has no ending");
        match terminal {
            crate::transcript::Record::RunFinished { outcome, .. } => {
                assert_eq!(outcome, "stopped")
            }
            _ => panic!("not a terminal"),
        }
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        assert!(fake.live().is_empty(), "a child was spawned after shutdown");
    }

    #[test]
    fn a_spawn_error_leaves_an_ending() {
        let root = scratch_root("engine-spawn-error-ending");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        fake.fail_spawn("the session process could not be started: fixture");
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let run_dir = wait_for("the run directory with a spawn-error ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            fake.last_run_dir()
                .filter(|dir| crate::transcript::terminal_record(dir).is_some())
        });
        assert!(
            crate::transcript::terminal_record(&run_dir).is_some(),
            "a spawn error left no ending"
        );
        assert!(fake.live().is_empty());
    }

    #[test]
    fn a_prepare_error_after_the_run_directory_gets_an_ending() {
        let root = scratch_root("engine-prepare-error-ending");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        fake.fail_prepare_after_dir("the workspace could not be prepared");
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        // The prepare failure is reported on the engine's next turn; keep the
        // engine turning until the ending appears.
        let run_dir = wait_for("the run directory with a prepare-error ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            fake.last_run_dir()
                .filter(|dir| crate::transcript::terminal_record(dir).is_some())
        });
        let terminal =
            crate::transcript::terminal_record(&run_dir).expect("a failed prepare left no ending");
        match terminal {
            crate::transcript::Record::RunFinished { outcome, .. } => {
                assert_eq!(outcome, "stopped")
            }
            _ => panic!("not a terminal"),
        }
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
        assert!(
            fake.live().is_empty(),
            "a child was spawned after the failure"
        );
    }

    #[test]
    fn a_prepare_error_with_the_agent_removed_still_gets_an_ending() {
        let root = scratch_root("engine-prepare-error-removed");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        fake.fail_prepare_after_dir("the workspace could not be prepared");
        let (prepared_rx, release) = fake.block_next_prepare();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        recv_within("the fake session's prepared run directory", &prepared_rx);
        let run_dir = fake.last_run_dir().expect("the run directory");
        {
            let mut inner = lock_inner(&engine.inner);
            inner.agents.clear();
        }
        let _ = release.send(());
        wait_until("the run's durable ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        let terminal = crate::transcript::terminal_record(&run_dir)
            .expect("a prepare error with the agent removed left no ending");
        match terminal {
            crate::transcript::Record::RunFinished { outcome, .. } => {
                assert_eq!(outcome, "stopped")
            }
            _ => panic!("not a terminal"),
        }
        assert!(run_dir.join(run::STOPPED_FILE).is_file());
    }

    #[test]
    fn a_spawn_error_after_the_pid_is_reported_still_gets_an_ending() {
        let root = scratch_root("engine-spawn-error-after-pid");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        fake.fail_spawn_after_block("the child could not be handed over");
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let _pid = recv_within("the fake session's reported child", &pid_rx);
        let run_dir = fake.last_run_dir().expect("the run directory");
        {
            let mut inner = lock_inner(&engine.inner);
            inner.agents.clear();
        }
        let _ = release.send(());
        wait_until("the run's durable ending", || {
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        assert!(
            crate::transcript::terminal_record(&run_dir).is_some(),
            "a spawn error after the pid report left no ending"
        );
    }

    #[test]
    fn a_removal_during_spawn_settles_the_child_it_already_created() {
        let root = scratch_root("engine-remove-session-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the fake session's child", &pid_rx);
        assert!(fake.is_live(pid));
        // The removal job changes settings before the in-flight spawn returns.
        {
            let mut inner = lock_inner(&engine.inner);
            assert!(inner.settings.remove_agent("agent-1"));
            assert!(inner.agent_index("agent-1").is_some());
            inner.agents.clear();
            inner.settings.next_agent = 2;
        }
        let _ = release.send(());
        let run_dir = fake.last_run_dir().expect("the run directory");
        wait_until("the run's durable ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        assert!(
            crate::transcript::terminal_record(&run_dir).is_some(),
            "a removed agent's spawned child left no durable ending"
        );
        assert!(
            !run_dir.join(crate::engine::session::PID_FILE).exists()
                && !run_dir.join(crate::run::PGID_FILE).exists(),
            "a removed agent's spawned child left identity handles"
        );
        assert!(!fake.is_live(pid), "the removed agent's child survived");
        assert!(
            engine.board().agents.is_empty(),
            "the removed agent came back"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_removal_fact_failure_stops_the_child_but_records_uncertainty() {
        let root = scratch_root("engine-remove-session-fact-fail");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the fake session's child", &pid_rx);
        let run_dir = fake.last_run_dir().expect("the run directory");
        fs::create_dir(run_dir.join(run::FIRST_ENDING_FILE)).expect("the obstructed fact");
        {
            let mut inner = lock_inner(&engine.inner);
            assert!(inner.settings.remove_agent("agent-1"));
            inner.agents.clear();
        }
        let _ = release.send(());
        wait_until("the uncertain ending fact", || {
            run_dir.join(run::ENDING_UNCERTAIN_FILE).is_file()
        });
        // `stop_uncertain` writes the uncertainty fact just before it signals and
        // waits for the child. Wait for that stop, not for the file alone.
        wait_until("the uncertain stop to end the child", || !fake.is_live(pid));
        assert!(!fake.is_live(pid), "the removed agent's child survived");
        assert!(run_dir.join(run::ENDING_UNCERTAIN_FILE).is_file());
        assert!(!run_dir.join(run::STOPPED_FILE).is_file());
        let error = run::recover_ending(&run_dir, Duration::from_millis(20))
            .expect_err("the uncertain run must not recover cleanly");
        assert!(error.contains("uncertain"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shutdown_returns_while_a_session_spawn_is_blocked_and_keeps_no_child() {
        let root = scratch_root("engine-quit-session-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the fake session's child", &pid_rx);
        assert!(fake.is_live(pid), "the fake child did not start");

        // A shutdown that waited for the blocked spawn would never return; the
        // liveness receive fails the test instead of hanging it.
        let (engine_tx, engine_rx) = std::sync::mpsc::channel();
        let shutdown_thread = std::thread::spawn(move || {
            let mut engine = engine;
            engine.shutdown(Duration::from_millis(200));
            let _ = engine_tx.send(engine);
        });
        let mut engine = recv_within("shutdown while a session spawn is blocked", &engine_rx);
        shutdown_thread.join().expect("the shutdown thread ends");
        let _ = release.send(());

        wait_until("the blocked spawn's child to be stopped", || {
            !fake.is_live(pid)
        });
        assert!(!fake.is_live(pid), "the session child was kept");
        assert!(
            engine.live_pids().is_empty(),
            "the engine still records {:?}",
            engine.live_pids()
        );
        let board = engine.board();
        assert!(matches!(board.agents[0].session, SessionState::Idle));
        let run_dir = fake.last_run_dir().expect("the spawned run directory");
        // `settle_ending` writes the durable ending on the session thread just after
        // the child is reaped; the check above is not a write barrier.
        wait_until("shutdown during spawn's durable ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        assert!(
            crate::transcript::terminal_record(&run_dir).is_some(),
            "shutdown during spawn left no durable ending"
        );
        assert!(
            !run_dir.join(crate::engine::session::PID_FILE).exists()
                && !run_dir.join(crate::run::PGID_FILE).exists(),
            "shutdown during spawn left identity handles"
        );
    }

    #[test]
    fn a_stop_during_a_spawn_settles_the_child_and_reopens_clean() {
        let root = scratch_root("engine-stop-session-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = recv_within("the fake session's child", &pid_rx);
        assert!(fake.is_live(pid));
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        let _ = release.send(());
        let run_dir = fake.last_run_dir().expect("the run directory");
        wait_until("the run's durable ending", || {
            tick(&mut engine, &clock, Duration::ZERO);
            crate::transcript::terminal_record(&run_dir).is_some()
        });
        assert!(
            crate::transcript::terminal_record(&run_dir).is_some(),
            "a stop during spawn left no durable ending"
        );
        assert!(
            !run_dir.join(crate::engine::session::PID_FILE).exists()
                && !run_dir.join(crate::run::PGID_FILE).exists(),
            "a stop during spawn left identity handles"
        );
        assert!(!fake.is_live(pid), "the called-off child survived");
        drop(engine);
        let settings = Settings::load(&settings::path_in(&root)).expect("settings");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut reopened = open_test_engine(&root, settings, clock, fake);
        assert!(
            reopened.board().agents[0].leftover.is_none(),
            "a settled stop during spawn blocked the reopen: {:?}",
            reopened.board().agents[0].leftover
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_runs_reason_is_the_cause_not_the_process_exit_line() {
        use std::os::unix::process::ExitStatusExt;

        let root = scratch_root("engine-failure-reason");
        let run_dir = root.join("run-1700000000000-0001");
        fs::create_dir_all(&run_dir).expect("the run directory");
        let mut transcript = crate::transcript::Transcript::open(&run_dir).expect("the transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0001",
                crate::run::RunOutcome::Failed,
                Some(1),
                1,
                Some(
                    "the model could not be reached: Connection refused (os error 111) — model x at http://127.0.0.1:1"
                        .to_owned(),
                ),
            ))
            .expect("the terminal record");
        fs::write(
            run_dir.join(LOG_FILE),
            "the model could not be reached: Connection refused (os error 111)\nagent process exiting with code 1\n",
        )
        .expect("the log");
        let status = std::process::Command::new("false")
            .status()
            .expect("false runs");
        let reason = session_failure_reason(&run_dir, status);
        assert!(
            reason.contains("Connection refused"),
            "the process-exit line won: {reason}"
        );
        assert!(!reason.contains("agent process exiting"), "{reason}");

        // A child killed by a signal with no terminal record names the kill, never a
        // stale tool-echo line from the log.
        let killed_dir = root.join("run-1700000000000-0002");
        fs::create_dir_all(&killed_dir).expect("the killed run directory");
        fs::write(
            killed_dir.join(LOG_FILE),
            "[turn 1 tool 1] sleep 60\nagent process exiting with code 1\n",
        )
        .expect("the killed log");
        let signal = std::process::ExitStatus::from_raw(9);
        assert_eq!(
            session_failure_reason(&killed_dir, signal),
            "run was killed"
        );
    }

    #[test]
    fn a_long_instruction_first_line_is_cut_on_a_word() {
        let root = scratch_root("engine-instruction-word");
        let path = root.join("instruction.md");
        // 112 cells, then a single 13-letter word whose sixth letter is the 120th
        // character: the old cut stored `extra` as a whole word and the renderer
        // then said `extraord…` at 80 columns (review B3).
        let prefix = "one two ".repeat(14);
        fs::write(&path, format!("{prefix}extraordinary result\n")).expect("the instruction");
        let (_chars, first, _text, _lines, _mtime) = instruction_facts(&path);
        let first = first.expect("a first line");
        assert!(first.ends_with('…'), "{first}");
        assert!(
            !first.contains("extra"),
            "the cut split `extraordinary`: {first}"
        );
        assert!(first.starts_with("one two one two"), "{first}");
        assert!(
            first.chars().count() <= 121,
            "the stored line is longer than the cap: {first}"
        );

        let short = "fix the flaky login test";
        fs::write(&path, format!("{short}\n")).expect("the short instruction");
        let (_chars, first, _text, _lines, _mtime) = instruction_facts(&path);
        assert_eq!(first.as_deref(), Some(short));
    }

    #[test]
    fn the_post_removal_line_fits_eighty_columns() {
        let line = crate::board::status::plain(&removed_note("agent-2"));
        assert!(line.contains("removed"), "{line}");
        assert!(line.contains("login deactivated"), "{line}");
        assert!(line.contains("history kept"), "{line}");
        assert!(
            line.chars().count() <= 78,
            "the line is {} chars: {line}",
            line.chars().count()
        );
    }

    #[test]
    fn a_clean_exits_terminal_record_decides_its_mark() {
        let root = scratch_root("engine-clean-ending");
        let run_dir = root.join("run-1700000000000-0001");
        fs::create_dir_all(&run_dir).expect("the run directory");
        let mut transcript = crate::transcript::Transcript::open(&run_dir).expect("the transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0001",
                crate::run::RunOutcome::NoWork,
                Some(0),
                1,
                Some("There are no open issues assigned to me.".to_owned()),
            ))
            .expect("the terminal record");
        assert_eq!(
            session_success_end(&run_dir),
            SessionEnd::NoWork {
                reason: "There are no open issues assigned to me.".to_owned()
            }
        );
        let refused_dir = root.join("run-1700000000000-0002");
        fs::create_dir_all(&refused_dir).expect("the second run");
        let mut transcript =
            crate::transcript::Transcript::open(&refused_dir).expect("the transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0002",
                crate::run::RunOutcome::Refused,
                Some(0),
                1,
                Some("I could not delete it.".to_owned()),
            ))
            .expect("the terminal record");
        assert!(matches!(
            session_success_end(&refused_dir),
            SessionEnd::Refused { .. }
        ));
        let warned_dir = root.join("run-1700000000000-0003");
        fs::create_dir_all(&warned_dir).expect("the third run");
        let mut transcript =
            crate::transcript::Transcript::open(&warned_dir).expect("the transcript");
        transcript
            .append(&crate::run::terminal_record_for_test(
                "run-1700000000000-0003",
                crate::run::RunOutcome::Warned,
                Some(0),
                1,
                Some("The review is complete.".to_owned()),
            ))
            .expect("the terminal record");
        assert_eq!(
            session_success_end(&warned_dir),
            SessionEnd::Warned {
                reason: "The review is complete.".to_owned()
            }
        );
    }

    #[test]
    fn the_repository_poll_is_short_enough_for_a_deletion_event() {
        assert!(
            REPOSITORY_REFRESH <= Duration::from_secs(2),
            "the board can stay stale for {REPOSITORY_REFRESH:?}"
        );
    }
    #[test]
    fn f2_a_failed_stop_save_still_stops_and_does_not_restart() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-f2-stop-save");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let settings_file = settings::path_in(&root);
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o400))
            .expect("the settings file is read-only");
        let result = engine.send(Command::Stop {
            name: "agent-1".to_owned(),
        });
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))
            .expect("the settings file is writable again");
        assert!(result.is_err(), "the failed save was reported as success");
        assert!(result.unwrap_err().contains("was stopped, but"));
        for _ in 0..5 {
            tick(&mut engine, &clock, Duration::from_secs(1));
        }
        assert_eq!(fake.counts().1, 1, "a stopped agent started a replacement");
        assert!(fake.live().is_empty());
        assert_eq!(engine.board().agents[0].desired, Desired::Stopped);
    }

    #[test]
    fn f2_a_failed_remove_save_still_stops_and_keeps_the_agent() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-f2-remove-save");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let settings_file = settings::path_in(&root);
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o400))
            .expect("the settings file is read-only");
        let result = engine.send(Command::RemoveAgent {
            name: "agent-1".to_owned(),
        });
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))
            .expect("the settings file is writable again");
        assert!(
            result.is_err(),
            "the failed removal was reported as success"
        );
        assert!(result.unwrap_err().contains("was stopped, but the removal"));
        for _ in 0..5 {
            tick(&mut engine, &clock, Duration::from_secs(1));
        }
        assert_eq!(fake.counts().1, 1, "a stopped agent started a replacement");
        assert!(fake.live().is_empty());
        assert_eq!(
            engine.board().agents.len(),
            1,
            "the agent was removed anyway"
        );
        assert_eq!(engine.board().agents[0].desired, Desired::Stopped);
    }

    #[test]
    fn f4r_a_marked_agent_is_not_scheduled_for_provisioning() {
        let root = scratch_root("engine-f4r-provision");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        options.forgejo_ready = false;
        let engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        {
            let mut inner = lock_inner(&engine.inner);
            inner.forgejo.state = ForgejoState::Running;
            inner.forgejo.provisioned = true;
            inner.forgejo.admin_token = Some(Secret::new("admin"));
            inner.agents[0].provisioning = Provisioning::Failed {
                reason: "cancelled".to_owned(),
            };
            inner.agents[0].removal_pending = false;
        }
        write_removal_marker(&lock_inner(&engine.inner), "agent-1").expect("marker");
        let jobs = lock_inner(&engine.inner).due_jobs(Instant::now());
        assert!(
            !jobs
                .iter()
                .any(|job| matches!(job, Job::ProvisionAgent(name) if name == "agent-1")),
            "a marked agent was scheduled for provisioning"
        );
    }

    #[test]
    fn f4r_a_reopen_restores_the_marked_block_and_never_provisions() {
        let root = scratch_root("engine-f4r-reopen");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let layout = Layout::rooted_at(root.clone()).expect("layout");
        let state_dir = layout
            .agent("agent-1")
            .expect("agent paths")
            .state_dir()
            .to_path_buf();
        std::fs::create_dir_all(&state_dir).expect("state dir");
        crate::mode::write_private(state_dir.join("removal-pending"), b"removal pending\n")
            .expect("marker");
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        options.forgejo_ready = false;
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_for_test(options, Arc::new(FakeClock::new()), fake.clone(), settings)
            .expect("the engine opens");
        {
            let inner = lock_inner(&engine.inner);
            assert!(matches!(
                inner.agents[0].session,
                SessionState::Blocked { .. }
            ));
        }
        {
            let mut inner = lock_inner(&engine.inner);
            inner.forgejo.state = ForgejoState::Running;
            inner.forgejo.provisioned = true;
            inner.forgejo.admin_token = Some(Secret::new("admin"));
            let jobs = inner.due_jobs(Instant::now());
            assert!(
                !jobs
                    .iter()
                    .any(|job| matches!(job, Job::ProvisionAgent(name) if name == "agent-1")),
                "a reopened marked agent was scheduled for provisioning"
            );
        }
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("Start returns without starting");
        assert_eq!(fake.counts().1, 0, "a reopened marked agent started");
    }

    #[test]
    fn f4t_removal_waits_for_an_inflight_provisioning_job() {
        let root = scratch_root("engine-f4t-serialized-identity");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let engine = open_test_engine(&root, settings, clock, fake);
        {
            let mut inner = lock_inner(&engine.inner);
            inner.agents[0].removal_pending = true;
            inner.agents[0].provision_inflight = true;
            let jobs = inner.due_jobs(Instant::now());
            assert!(
                !jobs
                    .iter()
                    .any(|job| matches!(job, Job::RemoveAgent(name) if name == "agent-1")),
                "removal ran while provisioning owned the identity"
            );
            inner.agents[0].provision_inflight = false;
            let jobs = inner.due_jobs(Instant::now());
            assert!(
                jobs.iter()
                    .any(|job| matches!(job, Job::RemoveAgent(name) if name == "agent-1")),
                "removal was not queued after provisioning finished"
            );
        }
    }

    #[test]
    fn f4s_an_active_removal_marker_failure_reports_the_stop() {
        use std::os::unix::fs::PermissionsExt;
        // The removal notice repeats the marker path and is capped at 600 bytes by
        // `forgejo_tools::sentence`. A long state root would cut it short, so own a
        // short root and still check every clause of the full notice below.
        let root = crate::test_support::short_scratch_root("engine-f4s-active-marker");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let state_dir = Layout::rooted_at(root.clone())
            .expect("layout")
            .agent("agent-1")
            .expect("agent paths")
            .state_dir()
            .to_path_buf();
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let marker = state_dir.join("removal-pending");
        std::fs::write(&marker, b"blocked\n").expect("marker");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o400))
            .expect("read-only marker");
        engine
            .send(Command::RemoveAgent {
                name: "agent-1".to_owned(),
            })
            .expect("RemoveAgent");
        tick(&mut engine, &clock, Duration::ZERO);
        let note = wait_for("the removal marker failure note", || {
            let inner = lock_inner(&engine.inner);
            inner
                .notice
                .clone()
                .filter(|note| note.contains("could not be removed"))
        });
        assert!(note.contains("was stopped and left stopped"), "{note}");
        assert!(
            note.contains("sign-in and account were not changed"),
            "{note}"
        );
        assert!(note.contains("start it again or retry Remove"), "{note}");
        assert!(
            !note.ends_with('…'),
            "the removal notice was cut before its recovery sentence: {note}"
        );
        let inner = lock_inner(&engine.inner);
        assert_eq!(inner.agents[0].desired, Desired::Stopped);
        assert!(inner.agents[0].running.is_none());
        assert!(!inner.agents[0].removal_pending);
        drop(inner);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn f4s_a_long_state_root_keeps_the_removal_recovery_clause() {
        use std::os::unix::fs::PermissionsExt;
        // The notice repeats the marker path and is capped at 600 bytes by
        // `forgejo_tools::sentence`. This state root is long enough to reach the
        // cap, so the recovery clause must come before the repeated path.
        let root = std::env::temp_dir().join(format!(
            "gewu-multiverse-test-engine-f4s-long-notice-{}-{}",
            std::process::id(),
            "a".repeat(200)
        ));
        fs::create_dir_all(&root).expect("the long state root");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let state_dir = Layout::rooted_at(root.clone())
            .expect("layout")
            .agent("agent-1")
            .expect("agent paths")
            .state_dir()
            .to_path_buf();
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let marker = state_dir.join("removal-pending");
        std::fs::write(&marker, b"blocked\n").expect("marker");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o400))
            .expect("read-only marker");
        engine
            .send(Command::RemoveAgent {
                name: "agent-1".to_owned(),
            })
            .expect("RemoveAgent");
        tick(&mut engine, &clock, Duration::ZERO);
        let note = wait_for("the removal marker failure note", || {
            let inner = lock_inner(&engine.inner);
            inner
                .notice
                .clone()
                .filter(|note| note.contains("could not be removed"))
        });
        assert!(
            note.len() <= 600,
            "the notice exceeded its documented bound: {} bytes",
            note.len()
        );
        assert!(
            note.ends_with('…'),
            "the long root did not reach the notice cap: {} bytes: {note}",
            note.len()
        );
        assert!(note.contains("was stopped and left stopped"), "{note}");
        assert!(
            note.contains("sign-in and account were not changed"),
            "{note}"
        );
        assert!(note.contains("start it again or retry Remove"), "{note}");
        let inner = lock_inner(&engine.inner);
        assert_eq!(inner.agents[0].desired, Desired::Stopped);
        assert!(inner.agents[0].running.is_none());
        assert!(!inner.agents[0].removal_pending);
        drop(inner);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn f4r_a_marker_write_failure_changes_nothing_and_start_still_runs() {
        let root = scratch_root("engine-f4r-marker-failure");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        {
            let mut inner = lock_inner(&engine.inner);
            inner.agents[0].removal_pending = true;
        }
        let state_dir = Layout::rooted_at(root.clone())
            .expect("layout")
            .agent("agent-1")
            .expect("agent paths")
            .state_dir()
            .to_path_buf();
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir).expect("clear state dir");
        }
        std::fs::write(&state_dir, b"not a directory").expect("blocking file");
        remove_agent_job(Arc::clone(&engine.inner), "agent-1".to_owned());
        {
            let inner = lock_inner(&engine.inner);
            assert_eq!(inner.agents.len(), 1);
            assert!(!inner.agents[0].removal_pending);
            assert!(
                !matches!(inner.agents[0].session, SessionState::Blocked { .. }),
                "a failed marker write left the agent blocked"
            );
            assert!(
                inner.notice.as_deref().is_some_and(|note| {
                    note.contains("was stopped and left stopped")
                        && note.contains("could not be removed")
                }),
                "{:?}",
                inner.notice
            );
        }
        std::fs::remove_file(&state_dir).expect("blocking file");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("Start after the failed marker");
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
    }

    #[test]
    fn f4_a_failed_removal_save_blocks_and_a_retry_completes() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-f4-removal");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        {
            let mut inner = lock_inner(&engine.inner);
            inner.agents[0].login = None;
        }
        let settings_file = settings::path_in(&root);
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o400))
            .expect("read-only settings");
        remove_agent_job(Arc::clone(&engine.inner), "agent-1".to_owned());
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))
            .expect("writable settings");
        {
            let inner = lock_inner(&engine.inner);
            assert_eq!(inner.agents.len(), 1);
            assert!(matches!(
                inner.agents[0].session,
                SessionState::Blocked { .. }
            ));
            assert!(
                inner
                    .notice
                    .as_deref()
                    .is_some_and(|note| note.contains("already cancelled"))
            );
        }
        assert!(
            engine
                .send(Command::Start {
                    name: "agent-1".to_owned()
                })
                .is_ok()
        );
        assert_eq!(fake.counts().1, 0, "a blocked cancelled identity started");
        remove_agent_job(Arc::clone(&engine.inner), "agent-1".to_owned());
        {
            let inner = lock_inner(&engine.inner);
            assert!(
                inner.agents.is_empty(),
                "the retry did not complete the removal"
            );
        }
    }

    #[test]
    fn f5_a_failed_remove_save_says_the_stop_will_not_survive_reopen() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-f5-remove-reopen");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        wait_started(&fake, 1);
        let settings_file = settings::path_in(&root);
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o400))
            .expect("read-only settings");
        let result = engine.send(Command::RemoveAgent {
            name: "agent-1".to_owned(),
        });
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))
            .expect("writable settings");
        let error = result.expect_err("the failed removal was reported as success");
        assert!(
            error.contains("will start again the next time the program opens"),
            "{error}"
        );
        drop(engine);
        let persisted = settings::Settings::load(&settings_file).expect("load");
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let mut reopened =
            open_test_engine(&root, persisted, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut reopened, &clock, Duration::ZERO);
        wait_started(&fake, 1);
    }

    #[test]
    fn f2_a_failed_endpoint_commit_restores_the_previous_key() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-f2-endpoint-key");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let layout = Layout::rooted_at(root.clone()).expect("layout");
        let credentials_path = layout.credentials_file();
        let before = credentials::model_key(&credentials_path, "")
            .expect("the previous key")
            .expose()
            .to_owned();
        let settings_file = settings::path_in(&root);
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o400))
            .expect("the settings file is read-only");
        let result = engine_send_with_settings(
            &root,
            settings,
            Command::SetEndpoint {
                url: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
                model: "new-model".to_owned(),
                key: Some("new-key".to_owned()),
            },
        );
        std::fs::set_permissions(&settings_file, std::fs::Permissions::from_mode(0o600))
            .expect("the settings file is writable again");
        assert!(result.is_err(), "the failed endpoint commit succeeded");
        let after = credentials::model_key(&credentials_path, "")
            .expect("the key after rollback")
            .expose()
            .to_owned();
        assert_eq!(after, before, "the new key was left on the old endpoint");
    }

    fn engine_send_with_settings(
        root: &Path,
        settings: Settings,
        command: Command,
    ) -> Result<(), String> {
        let mut options = test_options(root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )?;
        engine.send(command)
    }

    #[test]
    fn f1_a_failed_program_choice_is_not_persisted_by_the_next_save() {
        let root = scratch_root("engine-f1-program-rollback");
        let mut settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        settings.endpoint.url.clear();
        settings.endpoint.model.clear();
        settings
            .save(&settings::path_in(&root))
            .expect("the cleared endpoint saves");
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        assert_eq!(engine.board().agents[0].program, "builtin");
        let result = engine.send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "codex".to_owned(),
            protocol: None,
        });
        assert!(result.is_err(), "the unconfigured program choice succeeded");
        assert_eq!(engine.board().agents[0].program, "builtin");
        engine
            .send(Command::SetEndpoint {
                url: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
                model: "fixture".to_owned(),
                key: Some("fixture-key".to_owned()),
            })
            .expect("the later endpoint save");
        assert_eq!(
            engine.board().agents[0].program,
            "builtin",
            "the next save persisted the rejected program"
        );
    }

    #[test]
    fn a_failed_endpoint_key_leaves_the_live_endpoint_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-endpoint-key-failure");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let before = engine.board().endpoint.clone();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
            .expect("the root becomes read-only");
        let result = engine.send(Command::SetEndpoint {
            url: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
            model: "new-model".to_owned(),
            key: Some("new-key".to_owned()),
        });
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("the root is writable again");
        assert!(result.is_err(), "the failed key was reported as success");
        assert_eq!(engine.board().endpoint.url, before.url);
        assert_eq!(engine.board().endpoint.model, before.model);
    }

    #[test]
    fn c5_a_settings_save_failure_restores_the_prior_program_selection() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("engine-c5-settings-failure");
        let settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        engine
            .send(Command::SetAgentModel {
                name: "agent-1".to_owned(),
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                protocol: "chat-completions".to_owned(),
                model: "fixture".to_owned(),
                key: Some("fixture-key".to_owned()),
            })
            .expect("the fixture model saves");
        let before = engine.board().agents[0].program.clone();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
            .expect("the root becomes read-only");
        let result = engine.send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "pi".to_owned(),
            protocol: Some("chat-completions".to_owned()),
        });
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("the root is writable again");
        assert!(result.is_err(), "the save failure was reported as success");
        assert_eq!(engine.board().agents[0].program, before);
    }
    #[test]
    fn c6_key_snapshot_write_failure_rolls_back_program_and_other_agents() {
        let root = scratch_root("engine-c6-key-failure");
        let settings = write_ready(
            &root,
            &[("agent-1", Desired::Stopped), ("agent-2", Desired::Stopped)],
        );
        let layout = Layout::rooted_at(root.clone()).expect("layout");
        let mut options = test_options(&root);
        options.confinement = crate::confinement::Method::None;
        let mut engine = open_for_test(
            options,
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            settings,
        )
        .expect("the engine opens");
        let credentials = layout.credentials_file();
        let mut root: toml::Value =
            toml::from_str(&std::fs::read_to_string(&credentials).expect("credentials read"))
                .expect("credentials parse");
        root.as_table_mut().expect("table").remove("model_keys");
        crate::mode::write_private(
            &credentials,
            toml::to_string_pretty(&root)
                .expect("credentials serialise")
                .as_bytes(),
        )
        .expect("credentials without per-agent keys");
        credentials::record_secret(&credentials, None, "model_key", "fixture-default-key")
            .expect("default key");
        let before_one = engine.board().agents[0].program.clone();
        let before_two = engine.board().agents[1].program.clone();
        let before_provider = engine.inner.lock().expect("inner").settings.agents[0]
            .provider
            .clone();
        let before_other_provider = engine.inner.lock().expect("inner").settings.agents[1]
            .provider
            .clone();
        std::fs::set_permissions(
            &credentials,
            std::os::unix::fs::PermissionsExt::from_mode(0o400),
        )
        .expect("credentials become read-only");
        let result = engine.send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "pi".to_owned(),
            protocol: Some("chat-completions".to_owned()),
        });
        let _ = std::fs::set_permissions(
            &credentials,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        assert!(
            result.is_err(),
            "key snapshot failure was reported as success"
        );
        assert_eq!(engine.board().agents[0].program, before_one);
        assert_eq!(engine.board().agents[1].program, before_two);
        assert_eq!(
            engine.inner.lock().expect("inner").settings.agents[0].provider,
            before_provider
        );
        assert_eq!(
            engine.inner.lock().expect("inner").settings.agents[1].provider,
            before_other_provider
        );
        let saved = Settings::load(&settings::path_in(layout.root())).expect("durable settings");
        assert_eq!(saved.agents[0].harness(), Program::Builtin);
        assert_eq!(saved.agents[0].provider, before_provider);
        assert_eq!(saved.agents[1].provider, before_other_provider);
        assert!(
            credentials::agent_model_key(&credentials, "agent-1")
                .expect("key read")
                .is_none(),
            "an explicit agent key appeared after the failed snapshot"
        );
        assert_eq!(
            credentials::model_key(&credentials, "agent-1")
                .expect("default key")
                .expose(),
            "fixture-default-key"
        );
    }

    struct ResetTestArtifact;

    impl Drop for ResetTestArtifact {
        fn drop(&mut self) {
            crate::harness::set_test_artifact(None, None);
        }
    }

    #[test]
    fn shutdown_cancels_a_running_install_and_keeps_the_old_version() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::os::unix::fs::PermissionsExt;

        let root = scratch_root("engine-install-shutdown");
        let install = root.join("data/harnesses/pi/1.4.0");
        fs::create_dir_all(install.join("pi")).expect("the old install");
        let old_binary = install.join("pi/pi");
        fs::write(&old_binary, "#!/bin/sh\necho old-pi\n").expect("the old binary");
        fs::set_permissions(&old_binary, fs::Permissions::from_mode(0o755))
            .expect("the old binary mode");
        fs::write(
            install.join("install.json"),
            r#"{"program":"pi","version":"1.3.9","sha256":"fixture","license":"MIT","entrypoint":"pi/pi","resources":null,"installed_at":"2026-10-23T00:00:00.000Z"}"#,
        )
        .expect("the old record");

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let address = listener.local_addr().expect("the loopback address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let body = vec![3u8; 2 * 1024 * 1024];
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(header.as_bytes()).is_err() {
                return;
            }
            for part in body.chunks(4096) {
                if stream.write_all(part).is_err() || stream.flush().is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let _reset = ResetTestArtifact;
        crate::harness::set_test_artifact(
            Some(&format!("http://{address}/package")),
            Some(&"0".repeat(64)),
        );

        let mut engine = open_for_test(
            test_options(&root),
            Arc::new(FakeClock::new()),
            Arc::new(FakeSpawner::new()),
            Settings::default(),
        )
        .expect("the test engine opens");
        engine
            .send(Command::InstallProgram {
                id: "pi".to_owned(),
            })
            .expect("the install starts");
        let harness_dir = root.join("data/harnesses/pi");
        wait_until("the install download", || {
            fs::read_dir(&harness_dir).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    let stage = entry.path();
                    stage
                        .file_name()
                        .map(|name| name.to_string_lossy().starts_with(".stage-"))
                        .unwrap_or(false)
                        && fs::metadata(stage.join("pi.tar.part"))
                            .map(|metadata| metadata.len() > 0)
                            .unwrap_or(false)
                })
            })
        });

        engine.shutdown(Duration::from_secs(5));
        assert_eq!(
            fs::read_to_string(&old_binary).expect("the old binary"),
            "#!/bin/sh\necho old-pi\n"
        );
        let stages: Vec<_> = fs::read_dir(&harness_dir)
            .expect("the harness directory")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".stage-"))
            .collect();
        assert!(stages.is_empty(), "a canceled stage survived shutdown");
    }
    #[test]
    fn below_start_block_reasons_never_report_started() {
        type Mutation = fn(&mut Settings);
        let cases: [(&str, Mutation); 4] = [
            ("neg-provider", |settings| settings.endpoint.url.clear()),
            ("neg-key", |settings| {
                settings.agents[0].harness = "builtin".to_owned()
            }),
            ("neg-program", |settings| {
                settings.agents[0].harness = "codex".to_owned()
            }),
            ("neg-unknown", |settings| {
                settings.agents[0].harness = "potato".to_owned()
            }),
        ];
        for (name, mutate) in cases {
            let root = scratch_root(name);
            let mut settings = write_ready(&root, &[("agent-1", Desired::Stopped)]);
            mutate(&mut settings);
            settings
                .save(&settings::path_in(
                    Layout::rooted_at(root.clone())
                        .expect("the test layout is absolute")
                        .root(),
                ))
                .expect("settings");
            let clock = Arc::new(FakeClock::new());
            let fake = Arc::new(FakeSpawner::new());
            let mut engine = open_test_engine(&root, settings, clock, fake.clone());
            if name == "neg-key" {
                let _ = std::fs::remove_file(
                    Layout::rooted_at(root.clone())
                        .expect("layout")
                        .credentials_file(),
                );
            }
            engine
                .send(Command::Start {
                    name: "agent-1".to_owned(),
                })
                .expect("Start is accepted");
            assert_eq!(fake.counts().1, 0, "{name} started a session");
            let board = engine.board();
            match &board.agents[0].session {
                SessionState::Blocked { reason } => assert!(
                    reason.contains("model")
                        || reason.contains("installed")
                        || reason.contains("unknown"),
                    "{name}: {reason}"
                ),
                other => panic!("{name}: expected Blocked, got {other:?}"),
            }
            if name == "neg-unknown" {
                assert!(
                    board.agents[0].program_label.contains("potato"),
                    "{}",
                    board.agents[0].program_label
                );
            }
        }
    }
}
