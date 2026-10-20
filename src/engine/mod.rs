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
pub mod session;
pub mod settings;

use std::collections::VecDeque;
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
    ForgejoView, Provisioning, SessionEnd, SessionState,
};
use crate::forgejo::{DEFAULT_TIMEOUT, ForgejoClient, ForgejoError, Repository};
use crate::identity;
use crate::mode::{SECRET_FILE_MODE, set_mode};
use crate::paths::Layout;
use crate::run::process_alive;
use crate::run::{self, LOG_FILE};
use crate::transcript::{self, TRANSCRIPT_FILE};

use self::clock::{RealClock, SharedClock};
#[cfg(test)]
use self::session::FakeSpawner;
use self::session::{
    PreparedSession, RealSpawner, RunningSession, SessionRepository, SessionRequest, SessionSpawner,
};
use self::settings::{AgentSettings, MAX_AGENTS, Settings};

/// How long one session may run before the engine stops it.
pub const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How long a stopped process may take to end politely.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);

/// How long the bundled Forgejo may take to answer after it starts.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// The delay after a session that succeeded or ran at least this long.
pub const RESTART_DELAY: Duration = Duration::from_secs(3);

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

/// How often the organisation's repositories are listed again.
pub const REPOSITORY_REFRESH: Duration = Duration::from_secs(5);

/// The terms the first-run screen shows, one short item each. They are the product's
/// fixed words, in the first-time reader's language.
pub const TERMS: &[&str] = &[
    "Agents use this computer with your permissions.",
    "They can read your files, including saved keys.",
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
        Ok(Self {
            layout,
            data_root: data_root.into(),
            product_binary,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
            stop_grace: DEFAULT_STOP_GRACE,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            auto_start_forgejo: true,
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
    recover_stale_sessions(&options.layout, options.stop_grace)?;
    let mut inner = Inner::new(options.clone(), settings_path, settings, credentials)?;
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
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, error))
            if error == nix::errno::Errno::EWOULDBLOCK || error == nix::errno::Errno::EAGAIN =>
        {
            Err(format!(
                "another gewu-multiverse is already using {}; close it first",
                data_root.display()
            ))
        }
        Err((_, error)) => Err(format!("{} cannot be locked: {error}", path.display())),
    }
}

/// One running session.
struct RunningState {
    run_dir: PathBuf,
    started: Instant,
    handle: Box<dyn RunningSession>,
    stopping: bool,
    stop_deadline: Option<Instant>,
    stop_requested: bool,
    timed_out: bool,
}

/// One agent's engine state, beside its saved settings.
struct AgentRuntime {
    name: String,
    desired: Desired,
    provisioning: Provisioning,
    login: Option<String>,
    email: Option<String>,
    session: SessionState,
    sessions_since_start: u64,
    session_number: u64,
    recent: VecDeque<SessionEnd>,
    instruction_chars: usize,
    instruction_first_line: Option<String>,
    instruction_mtime: Option<SystemTime>,
    activity: Option<String>,
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
}

/// The bundled Forgejo's engine state.
struct ForgejoRuntime {
    state: ForgejoState,
    url: String,
    admin_login: String,
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
    notice: Option<String>,
    admin_password_once: Option<String>,
    sequence: u64,
    stopping: bool,
    generation: u64,
}

impl Inner {
    fn new(
        options: EngineOptions,
        settings_path: PathBuf,
        settings: Settings,
        credentials: PathBuf,
    ) -> Result<Self, String> {
        let binary = bundled_forgejo::binary_path(&options.data_root);
        let paths = bundled_forgejo::InstancePaths::under(&options.data_root);
        let port = fs::read_to_string(&paths.app_ini)
            .ok()
            .and_then(|text| bundled_forgejo::port_from_config(&text));
        let admin_login = if !settings.forgejo.admin_login.trim().is_empty() {
            settings.forgejo.admin_login.clone()
        } else {
            fs::read_to_string(&paths.admin_login_file)
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| bundled_forgejo::valid_username(text))
                .unwrap_or_else(bundled_forgejo::admin_login_from_env)
        };
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
        let mut forgejo = ForgejoRuntime {
            state,
            url: port
                .map(bundled_forgejo::InstancePaths::url)
                .unwrap_or_default(),
            admin_login,
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
            notice: None,
            admin_password_once: None,
            sequence: 0,
            stopping: false,
            generation: 0,
        };
        let names = inner.settings.names();
        for name in names {
            inner.agents.push(AgentRuntime::load(&inner, &name));
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

    fn agent_index(&self, name: &str) -> Option<usize> {
        self.agents.iter().position(|agent| agent.name == name)
    }

    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notice = Some(crate::forgejo_tools::sentence(text.into()));
    }

    /// The saved choice for one agent, kept in step with the settings.
    fn set_desired(&mut self, index: usize, desired: Desired) {
        self.agents[index].desired = desired;
        if let Some(saved) = self.settings.agent_mut(&self.agents[index].name) {
            saved.set_desired(desired);
        }
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
        let instruction = paths
            .as_ref()
            .map(|paths| paths.instruction().to_path_buf());
        let (chars, first_line, mtime) = instruction
            .as_deref()
            .map(instruction_facts)
            .unwrap_or((0, None, None));
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
        Self {
            name: name.to_owned(),
            desired,
            provisioning,
            login,
            email,
            session: SessionState::Idle,
            sessions_since_start: 0,
            session_number: 0,
            recent,
            instruction_chars: chars,
            instruction_first_line: first_line,
            instruction_mtime: mtime,
            activity,
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
        let (chars, first_line, mtime) = instruction_facts(paths.instruction());
        self.instruction_chars = chars;
        self.instruction_first_line = first_line;
        self.instruction_mtime = mtime;
    }
}

fn end_from_outcome(summary: &run::RunSummary) -> Option<SessionEnd> {
    match summary.outcome? {
        run::RunOutcome::Succeeded => Some(SessionEnd::Succeeded),
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

fn instruction_facts(path: &Path) -> (usize, Option<String>, Option<SystemTime>) {
    let mtime = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok());
    let Ok(bytes) = fs::read(path) else {
        return (0, None, mtime);
    };
    if agent_run::instruction_is_blank(&bytes) {
        return (0, None, mtime);
    }
    let text = String::from_utf8_lossy(&bytes);
    let chars = text.chars().count();
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            let full = line.chars().count();
            let mut line: String = line.chars().take(120).collect();
            if full > 120 {
                line.push('…');
            }
            line
        });
    (chars, first, mtime)
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
                pids.push(run.handle.pid());
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
        let mut ended: Option<(SessionEnd, Instant)> = None;
        if let Some(run) = self.agents[index].running.as_mut() {
            if !run.stopping {
                if now.saturating_duration_since(run.started) >= self.options.session_timeout {
                    run.timed_out = true;
                    run.stopping = true;
                    run.stop_deadline = Some(now + self.options.stop_grace);
                    run.handle.signal(false);
                }
            } else if run.stop_deadline.is_some_and(|deadline| now >= deadline) {
                run.handle.signal(true);
                run.stop_deadline = Some(now + Duration::from_secs(1));
            }
            if let Some(status) = run.handle.try_status() {
                let end = if desired == Desired::Stopped || run.stop_requested {
                    SessionEnd::Stopped
                } else if run.timed_out {
                    SessionEnd::TimedOut
                } else if status.success() {
                    SessionEnd::Succeeded
                } else {
                    SessionEnd::Failed {
                        reason: session_failure_reason(&run.run_dir, status),
                    }
                };
                ended = Some((end, run.started));
            }
        }
        if let Some((end, started)) = ended {
            self.agents[index].running = None;
            self.agents[index].starting = false;
            self.agents[index].activity_refresh_at = None;
            let agent = &mut self.agents[index];
            finish_session(agent, end, now, started);
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
                }
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
        if !self.settings.endpoint.configured() || !self.credential_key_present() {
            return Some("needs a model".to_owned());
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
                if self.agents[index].running.is_none() && !self.agents[index].starting && due {
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
        let agent = &self.agents[index];
        let paths = self.options.layout.agent(&agent.name).ok()?;
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
            agent: agent.name.clone(),
            run_id,
            run_dir,
            state_root: self.options.layout.state_root().to_path_buf(),
            workspace: paths.workspace().to_path_buf(),
            instruction: paths.instruction().to_path_buf(),
            skills_dir: paths.skills_dir().to_path_buf(),
            endpoint: self.settings.endpoint.url.clone(),
            model: self.settings.endpoint.model.clone(),
            max_turns: self.settings.endpoint.turns(),
            timeout_secs: self.options.session_timeout.as_secs(),
            credentials: self.credentials.clone(),
            login,
            email,
            forgejo_url: self.forgejo.url.clone(),
            repositories,
        })
    }
}

fn backoff_for_restart(failures: u32) -> Duration {
    let factor = 1u64 << failures.saturating_sub(1).min(5);
    FORGEJO_RESTART_BASE
        .saturating_mul(factor as u32)
        .min(FORGEJO_RESTART_MAX)
}

fn session_failure_reason(run_dir: &Path, status: std::process::ExitStatus) -> String {
    if let Some(sentence) = last_run_log_line(run_dir) {
        return sentence;
    }
    match status.code() {
        Some(code) => format!("the session exited with code {code}"),
        None => "the session was ended by a signal".to_owned(),
    }
}

fn last_run_log_line(run_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(run_dir.join(LOG_FILE)).ok()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(crate::forgejo_tools::sentence(line.trim().to_owned()))
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

fn finish_session(agent: &mut AgentRuntime, mut end: SessionEnd, now: Instant, started: Instant) {
    if agent.desired == Desired::Stopped {
        end = SessionEnd::Stopped;
    }
    let ran = now.saturating_duration_since(started);
    agent.recent.push_back(end.clone());
    while agent.recent.len() > 10 {
        agent.recent.pop_front();
    }
    agent.last_end = Some(end.clone());
    if let Some(dir) = agent.newest_run.clone() {
        if let Some(action) = run::forgejo_actions(&dir).pop() {
            agent.activity = Some(plain_activity(&action.sentence));
        }
    }
    if agent.desired == Desired::Stopped || end == SessionEnd::Stopped {
        agent.session = SessionState::Idle;
        agent.next_at = None;
        return;
    }
    let delay = if end == SessionEnd::Succeeded || ran >= LONG_SESSION {
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
            model: inner.settings.endpoint.model.clone(),
            key_present: inner.credential_key_present(),
            test: inner.endpoint_test.clone(),
        };
        let terms_acknowledged = inner.settings.terms_acknowledged;
        let setup_complete =
            inner.settings.endpoint.configured() && !inner.settings.agents.is_empty();
        let forgejo = ForgejoView {
            state: inner.forgejo.state.clone(),
            url: inner.forgejo.url.clone(),
            version: bundled_forgejo::VERSION.to_owned(),
            admin_login: inner.forgejo.admin_login.clone(),
            organisation: bundled_forgejo::ORGANISATION.to_owned(),
            repositories: inner.forgejo.repositories.len(),
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
                recent: agent.recent.iter().cloned().collect(),
                instruction_chars: agent.instruction_chars,
                instruction_first_line: agent.instruction_first_line.clone(),
                activity: agent.activity.clone(),
            })
            .collect();
        BoardView {
            terms_acknowledged,
            terms: TERMS.iter().map(|term| (*term).to_owned()).collect(),
            setup_complete,
            forgejo,
            agents,
            endpoint,
            notice,
            admin_password,
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
                inner.settings.terms_acknowledged = true;
                inner.save_settings()?;
                inner.note("the terms are recorded");
                Ok(())
            }
            Command::SetEndpoint { url, model, key } => {
                let url = url.trim().to_owned();
                let model = model.trim().to_owned();
                inner.settings.endpoint.url = url;
                inner.settings.endpoint.model = model;
                if let Some(key) = key.filter(|key| !key.trim().is_empty()) {
                    credentials::record_secret(&inner.credentials, None, "model_key", key.trim())
                        .map_err(|error| format!("the model key could not be kept: {error}"))?;
                }
                inner.save_settings()?;
                inner.endpoint_test = EndpointTest::NotRun;
                inner.note("your model is saved");
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
                for number in 1..=count {
                    let name = format!("agent-{number}");
                    if inner.settings.agent(&name).is_none() {
                        inner.settings.add_agent(&name, Desired::Stopped)?;
                        ensure_agent_paths(&inner.options.layout, &name)?;
                        let runtime = AgentRuntime::load(&inner, &name);
                        inner.agents.push(runtime);
                    }
                }
                inner.save_settings()?;
                let count = inner.agents.len();
                inner.note(format!("{count} agents are ready"));
                Ok(())
            }
            Command::AddAgent => {
                let name = inner.settings.next_free_name();
                inner.settings.add_agent(&name, Desired::Stopped)?;
                ensure_agent_paths(&inner.options.layout, &name)?;
                let runtime = AgentRuntime::load(&inner, &name);
                inner.agents.push(runtime);
                inner.save_settings()?;
                inner.note(format!("`{name}` was added"));
                Ok(())
            }
            Command::RemoveAgent { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                inner.set_desired(index, Desired::Stopped);
                inner.agents[index].removal_pending = true;
                inner.agents[index].removal_retry_at = None;
                let grace = inner.options.stop_grace;
                stop_agent(&mut inner.agents[index], grace);
                inner.save_settings()?;
                inner.note(format!("`{name}` is being removed"));
                Ok(())
            }
            Command::Start { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                inner.set_desired(index, Desired::Started);
                let agent = &mut inner.agents[index];
                agent.sessions_since_start = 0;
                agent.session_number = 0;
                agent.quick_failures = 0;
                agent.next_at = None;
                inner.save_settings()?;
                inner.note(format!("`{name}` started"));
                Ok(())
            }
            Command::Stop { name } => {
                let Some(index) = inner.agent_index(&name) else {
                    return Err(format!("there is no agent named `{name}`"));
                };
                inner.set_desired(index, Desired::Stopped);
                let grace = inner.options.stop_grace;
                stop_agent(&mut inner.agents[index], grace);
                inner.save_settings()?;
                inner.note(format!("`{name}` stopped"));
                Ok(())
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
                let paths = bundled_forgejo::InstancePaths::under(&inner.options.data_root);
                let password = fs::read_to_string(&paths.admin_password)
                    .map_err(|_| "the administrator password is not there yet".to_owned())?;
                inner.admin_password_once = Some(password.trim().to_owned());
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

    fn shutdown(&mut self, grace: Duration) {
        let started = Instant::now();
        {
            let mut inner = lock_inner(&self.inner);
            if inner.stopping {
                return;
            }
            inner.stopping = true;
            inner.generation = inner.generation.saturating_add(1);
            // Ask every group politely. A start or spawn still in flight records its
            // child's pid, and is signalled again as that pid appears below.
            for agent in &mut inner.agents {
                if let Some(run) = agent.running.as_mut() {
                    run.stopping = true;
                    run.handle.signal(false);
                }
                if let Some(pid) = agent.starting_pid {
                    signal_group(pid, nix::sys::signal::Signal::SIGTERM);
                }
            }
            if let Some(server) = inner.forgejo.child.as_mut() {
                signal_group(server.pid(), nix::sys::signal::Signal::SIGTERM);
            }
            if let Some(pid) = inner.forgejo.start_pid {
                signal_group(pid, nix::sys::signal::Signal::SIGTERM);
            }
            for pid in &inner.forgejo.helper_pids {
                signal_group(*pid, nix::sys::signal::Signal::SIGTERM);
            }
        }
        // The bounded hand-over: a start or spawn that is between spawning its child
        // and returning must see the new generation and stop that child itself. The
        // engine lock is not held here, so that work can finish. Pids that appear
        // during the wait are signalled as soon as they are visible.
        let mut signalled: Vec<u32> = Vec::new();
        loop {
            let mut appeared: Vec<u32> = Vec::new();
            let mut alive = false;
            {
                let mut inner = lock_inner(&self.inner);
                for agent in &mut inner.agents {
                    if let Some(run) = agent.running.as_mut() {
                        if run.handle.try_status().is_some() {
                            agent.running = None;
                        } else {
                            alive = true;
                        }
                    }
                    if agent.starting {
                        alive = true;
                        if let Some(pid) = agent.starting_pid {
                            appeared.push(pid);
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
            for pid in appeared {
                if !signalled.contains(&pid) {
                    signalled.push(pid);
                    signal_group(pid, nix::sys::signal::Signal::SIGTERM);
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
                    stop_running_session(&mut run.handle, Duration::from_millis(500));
                }
                if let Some(pid) = agent.starting_pid.take() {
                    signal_group(pid, nix::sys::signal::Signal::SIGKILL);
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
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            }
            for pid in std::mem::take(&mut inner.forgejo.helper_pids) {
                // The token helper's job owns the wait and reaping; the force below
                // ends it if it is still in the grace window's shadow.
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            }
            inner.forgejo.state = ForgejoState::Stopped;
            inner.forgejo.admin_token = None;
            inner.forgejo.start_requested = false;
            inner.forgejo.start_inflight = false;
            inner.forgejo.provisioned = false;
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
    }
}

/// Take the engine state lock, recovering from a poisoned holder.
fn lock_inner(inner: &Arc<Mutex<Inner>>) -> std::sync::MutexGuard<'_, Inner> {
    match inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Stop one session handle politely, then by force, and wait for its status so the
/// child is reaped before the handle is dropped.
fn stop_running_session(run: &mut Box<dyn RunningSession>, grace: Duration) {
    if run.try_status().is_some() {
        return;
    }
    run.signal(false);
    if wait_session_status(run, grace) {
        return;
    }
    run.signal(true);
    let _ = wait_session_status(run, Duration::from_secs(1));
}

fn wait_session_status(run: &mut Box<dyn RunningSession>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if run.try_status().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Signal a process group by the pid that leads it. A pid that is already gone is
/// not an error: the job that owns it reaps it and notices the generation change.
fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
}

impl Drop for Engine {
    fn drop(&mut self) {
        let grace = self.options.stop_grace;
        self.shutdown(grace);
    }
}

/// Stop every `--run-agent` process an earlier program left behind.
///
/// The state root is walked without following the agents' own settings: a run
/// directory is two levels down, and its pid record is believed only when the live
/// command line still names that run directory and the run-agent mode.
fn recover_stale_sessions(layout: &Layout, grace: Duration) -> Result<Vec<u32>, String> {
    let mut stopped = Vec::new();
    let Ok(agents) = fs::read_dir(layout.state_root()) else {
        return Ok(stopped);
    };
    for agent in agents.flatten() {
        let agent_path = agent.path();
        if !agent_path.is_dir() {
            continue;
        }
        let Ok(runs) = fs::read_dir(&agent_path) else {
            continue;
        };
        for run in runs.flatten() {
            let run_dir = run.path();
            if !run_dir.is_dir() {
                continue;
            }
            let record = run_dir.join(session::PID_FILE);
            let Ok(text) = fs::read_to_string(&record) else {
                continue;
            };
            let Ok(pid) = text.trim().parse::<u32>() else {
                let _ = fs::remove_file(&record);
                continue;
            };
            let expected = run_dir.display().to_string();
            let matches = bundled_forgejo::command_line(pid).is_some_and(|parts| {
                parts.iter().any(|part| part == "--run-agent")
                    && parts.iter().any(|part| part == &expected)
            });
            if !matches {
                let _ = fs::remove_file(&record);
                continue;
            }
            if !bundled_forgejo::stop_recorded_pid(pid, grace) {
                return Err(format!(
                    "the run left by an earlier start (pid {pid}) could not be stopped"
                ));
            }
            let _ = fs::remove_file(&record);
            stopped.push(pid);
        }
    }
    Ok(stopped)
}

fn ensure_agent_paths(layout: &Layout, name: &str) -> Result<(), String> {
    layout
        .ensure_agent(name)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn stop_agent(agent: &mut AgentRuntime, grace: Duration) {
    agent.next_at = None;
    agent.session = SessionState::Idle;
    if agent.starting {
        // The job checks the desired choice after every wait, stops a child it has
        // already spawned, and clears `starting` itself.
        if let Some(pid) = agent.starting_pid {
            signal_group(pid, nix::sys::signal::Signal::SIGTERM);
        }
        agent.starting = false;
    }
    if let Some(run) = agent.running.as_mut() {
        run.stopping = true;
        run.stop_requested = true;
        run.stop_deadline = Some(Instant::now() + grace);
        run.handle.signal(false);
    }
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
            }
        } else {
            bundled_forgejo::prepare_instance_checked(&data_root, &binary, &admin_login, &cancel)?
        };
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
        inner.forgejo.repositories_refreshed_at = Some(Instant::now());
        if let Ok(repositories) = result {
            inner.forgejo.repositories = repositories;
        }
    }
}

fn provision_agent_job(inner: Arc<Mutex<Inner>>, name: String) {
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
            return;
        };
        if guard.stopping
            || guard.generation != generation
            || guard.agents[index].desired != Desired::Started
            || !guard.agents[index].starting
        {
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
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
            let agent = &mut guard.agents[index];
            finish_session(agent, SessionEnd::Failed { reason }, now, started);
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
    let spawned = spawner.spawn_with(&prepared, &mut on_spawn);
    let now = clock.now();
    let mut guard = lock_inner(&inner);
    let Some(index) = guard.agent_index(&request.agent) else {
        drop(guard);
        if let Ok(mut handle) = spawned {
            stop_running_session(&mut handle, Duration::from_secs(1));
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
        drop(guard);
        if let Ok(mut handle) = spawned {
            // This job owns the child it has just spawned. Stop and reap it; the
            // shutdown that called it off may have signalled the group already.
            stop_running_session(&mut handle, Duration::from_secs(1));
        }
        return;
    }
    let started = guard.agents[index].starting_since.unwrap_or(now);
    match spawned {
        Ok(handle) => {
            let _pid = handle.pid();
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            guard.agents[index].running = Some(RunningState {
                run_dir: request.run_dir.clone(),
                started,
                handle,
                stopping: false,
                stop_deadline: None,
                stop_requested: false,
                timed_out: false,
            });
            guard.agents[index].newest_run = Some(request.run_dir.clone());
            guard.agents[index].activity_refresh_at = None;
            guard.agents[index].session = SessionState::Running {
                number: guard.agents[index].session_number,
                since: SystemTime::now(),
            };
        }
        Err(reason) => {
            guard.agents[index].starting = false;
            guard.agents[index].starting_pid = None;
            let _ = transcript::record_outcome_if_missing(
                &request.run_dir,
                &request.run_id,
                run::RunOutcome::Failed,
                Some(reason.clone()),
                0,
            );
            let agent = &mut guard.agents[index];
            finish_session(agent, SessionEnd::Failed { reason }, now, started);
        }
    }
}

fn remove_agent_job(inner: Arc<Mutex<Inner>>, name: String) {
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
                inner.agents.remove(index);
                inner.settings.remove_agent(&name);
                if let Err(error) = inner.save_settings() {
                    inner.note(format!("`{name}` was removed, but {error}"));
                } else {
                    inner.note(format!(
                        "`{name}` was removed; its Forgejo account and history stay"
                    ));
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

/// One endpoint failure in the words the model screen uses. No HTTP status, raw body
/// or engine word reaches the person.
fn plain_endpoint_failure(error: &crate::model::ModelError, model: &str) -> String {
    use crate::model::ModelError;
    match error {
        ModelError::BadUrl { .. }
        | ModelError::RouteNotFound { .. }
        | ModelError::RouteNotFoundListed { .. } => {
            "wrong address — it should end in /chat/completions".to_owned()
        }
        ModelError::MissingKey | ModelError::BadKey { .. } => "the API key was refused".to_owned(),
        ModelError::ModelNotFound { .. } => {
            format!("no model called {model} at this address")
        }
        ModelError::Timeout { secs } => format!("no answer within {secs} s"),
        ModelError::RateLimited { .. } => "the model is busy — try again".to_owned(),
        ModelError::Server { .. } => "the model answered with an error".to_owned(),
        ModelError::MalformedReply { .. } => "the model's answer could not be read".to_owned(),
        ModelError::Transport { .. } => {
            "the model could not be reached — check the internet connection".to_owned()
        }
    }
}

fn sentence(error: &impl std::fmt::Display) -> String {
    crate::forgejo_tools::sentence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::clock::FakeClock;
    use crate::test_support::scratch_root;

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
        let deadline = Instant::now() + Duration::from_secs(2);
        while fake.counts().1 < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(fake.counts().1, count, "the session did not start");
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
                "They can read your files, including saved keys.",
                "Each has its own login on your Forgejo.",
                "Started agents keep going until you stop them.",
                "Their work is sent to your model provider.",
            ]
        );
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
                ModelError::BadKey { status: 401 },
                "the API key was refused",
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
            for banned in ["HTTP", "endpoint", "session", "token", "2xx"] {
                assert!(!sentence.contains(banned), "{sentence} has {banned}");
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
    fn a_session_that_succeeds_restarts_after_three_seconds() {
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
        tick(&mut engine, &clock, Duration::from_secs(2));
        assert_eq!(fake.counts().1, 1, "a session started before the 3 s delay");
        tick(&mut engine, &clock, Duration::from_secs(1));
        wait_started(&fake, 2);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
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

    #[test]
    fn a_long_failure_resets_the_backoff_to_three_seconds() {
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
        tick(&mut engine, &clock, Duration::from_secs(2));
        assert_eq!(fake.counts().1, started, "a long failure did not wait 3 s");
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
        tick(&mut engine, &clock, RESTART_DELAY);
        wait_started(&fake, 2);
        fake.finish_current(0);
        tick(&mut engine, &clock, Duration::ZERO);
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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let board = engine.board();
            match board.endpoint.test {
                EndpointTest::Passed { answer } => {
                    assert_eq!(answer, "the model answered");
                    break;
                }
                EndpointTest::Failed { reason } => panic!("the probe failed: {reason}"),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "the endpoint test did not finish"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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
        let job = std::thread::spawn(move || {
            ensure_admin_token(&token_inner, &helper, &root, "tester", &move || {
                lock_inner(&cancel_inner).stopping
            })
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_file.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the token helper wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(process_alive(pid), "the token helper did not start");

        let began = Instant::now();
        engine.shutdown(Duration::from_millis(200));
        let elapsed = began.elapsed();
        let result = job.join().expect("the token job ends");
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
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown waited {elapsed:?} for the token helper"
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
        let pid = entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the fake server reached its child");
        assert!(process_alive(pid), "the fake child did not start");

        let began = Instant::now();
        engine.shutdown(Duration::from_millis(250));
        let waited = began.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "shutdown waited {waited:?} on an in-flight start"
        );
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
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("the start job exits after shutdown");
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
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
    fn shutdown_returns_while_a_session_spawn_is_blocked_and_keeps_no_child() {
        let root = scratch_root("engine-quit-session-spawn");
        let settings = write_ready(&root, &[("agent-1", Desired::Started)]);
        let clock = Arc::new(FakeClock::new());
        let fake = Arc::new(FakeSpawner::new());
        let (pid_rx, release) = fake.block_next_spawn();
        let mut engine = open_test_engine(&root, settings, Arc::clone(&clock), Arc::clone(&fake));
        tick(&mut engine, &clock, Duration::ZERO);
        let pid = pid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the fake session reached its child");
        assert!(fake.is_live(pid), "the fake child did not start");

        let began = Instant::now();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (engine_tx, engine_rx) = std::sync::mpsc::channel();
        let shutdown_thread = std::thread::spawn(move || {
            let mut engine = engine;
            engine.shutdown(Duration::from_millis(200));
            let _ = done_tx.send(());
            let _ = engine_tx.send(engine);
        });
        match done_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => assert!(
                began.elapsed() < Duration::from_secs(1),
                "shutdown took {:?} while a spawn was blocked",
                began.elapsed()
            ),
            Err(_) => {
                let _ = release.send(());
                let _ = done_rx.recv_timeout(Duration::from_secs(5));
                panic!("shutdown blocked on the in-flight session spawn");
            }
        }
        let mut engine = engine_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the engine comes back after shutdown");
        let _ = shutdown_thread.join();
        let _ = release.send(());

        let deadline = Instant::now() + Duration::from_secs(2);
        while fake.is_live(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!fake.is_live(pid), "the session child was kept");
        assert!(
            engine.live_pids().is_empty(),
            "the engine still records {:?}",
            engine.live_pids()
        );
        let board = engine.board();
        assert!(matches!(board.agents[0].session, SessionState::Idle));
    }
}
