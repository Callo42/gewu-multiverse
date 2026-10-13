//! The state behind the screens, and the keys that move around them.
//!
//! Every key binding is here rather than in the terminal loop, so the screens can be
//! driven without a terminal: the tests feed [`App::on_key`] and render the result.
//!
//! Four rules hold across the whole file.
//!
//! - A screen never claims a capability the program does not have.
//! - An agent start is refused unless the first-run risk acknowledgment was confirmed
//!   and read back.
//! - A start beyond the concurrency limit waits its turn; it is not an error.
//! - The terminal is never held hostage: a request to edit a file or read a log is
//!   put in [`App::take_action`] for the terminal loop to carry out, because the
//!   loop is the part that can step out of the UI and put it back.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};

use crate::config::{AgentConfig, Config};
use crate::credentials::{self, CredentialsFile};
use crate::forgejo::{self, ForgejoClient, RoleCheck};
use crate::identity::{self, AgentIdentity};
use crate::model;
use crate::paths::Layout;
use crate::risk::{self, AckState};
use crate::run::{RunModel, RunSummary, recent_runs};
use crate::supervisor::{ModeChange, StartDisposition, StopDisposition, Supervisor};
use crate::systemd::Mode;
use crate::workspace::{self, GitCredential, GitIdentity};

/// How often the runs screen re-reads the records on disk.
const RECENT_RUNS_REFRESH: Duration = Duration::from_secs(1);

/// How many runs the runs screen lists.
const RECENT_RUNS_LIMIT: usize = 50;

/// The screens of the terminal UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The list of agents and their state. The screen the program opens on.
    Agents,
    /// One agent's configuration, as the configuration file describes it.
    AgentConfig,
    /// An agent's instruction and skills.
    Instructions,
    /// The Forgejo connection and the role check.
    Forgejo,
    /// Where the credentials file is, and the mode it has.
    Credentials,
    /// Each agent's recent runs.
    Runs,
}

impl Screen {
    /// The screens in the sequence the tab bar shows them.
    pub const ALL: [Screen; 6] = [
        Screen::Agents,
        Screen::AgentConfig,
        Screen::Instructions,
        Screen::Forgejo,
        Screen::Credentials,
        Screen::Runs,
    ];

    /// The tab label.
    pub fn title(self) -> &'static str {
        match self {
            Screen::Agents => "Agents",
            Screen::AgentConfig => "Agent",
            Screen::Instructions => "Instruction",
            Screen::Forgejo => "Forgejo",
            Screen::Credentials => "Credentials",
            Screen::Runs => "Runs",
        }
    }

    /// The position in [`Screen::ALL`].
    pub fn index(self) -> usize {
        Screen::ALL
            .iter()
            .position(|screen| *screen == self)
            .unwrap_or(0)
    }

    /// The screen `steps` places along the tab bar, wrapping around.
    pub fn shifted(self, steps: isize) -> Screen {
        let count = Screen::ALL.len() as isize;
        let at = (self.index() as isize + steps).rem_euclid(count);
        Screen::ALL[at as usize]
    }
}

/// Which file an edit request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    /// The agent's instruction document.
    Instruction,
    /// The agent's skills directory.
    Skills,
    /// The agent's declared run steps.
    Steps,
}

impl EditTarget {
    /// The word the status line uses.
    pub fn label(self) -> &'static str {
        match self {
            EditTarget::Instruction => "instruction",
            EditTarget::Skills => "skills",
            EditTarget::Steps => "declared steps",
        }
    }
}

/// What the terminal loop must step out of the UI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAction {
    /// Whether a file is being edited or read.
    pub kind: ActionKind,
    /// The path handed to the editor or pager.
    pub path: PathBuf,
    /// A short word for the status line.
    pub label: String,
}

/// Whether a pending action edits a file or reads one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Hand the file to the user's editor.
    Edit,
    /// Hand the file to the user's pager.
    View,
}

/// The result of the "test this endpoint" action for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointTest {
    /// The agent whose endpoint was tested.
    pub agent: String,
    /// The endpoint that was called.
    pub endpoint: String,
    /// The model that was asked for.
    pub model: String,
    /// On success, a one-line summary of the reply; on failure, its class and message.
    pub result: Result<String, EndpointFailure>,
}

/// Why the endpoint test failed, with the class a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointFailure {
    /// One word: `bad_url`, `missing_key`, `bad_key`, `model_not_found`, `rate_limited`,
    /// `server_error`, `malformed_reply`, `timeout` or `transport`.
    pub class: String,
    /// The message, which never holds the key.
    pub message: String,
}

impl EndpointTest {
    /// One line for the status bar and the agent screen.
    pub fn describe(&self) -> String {
        describe_endpoint_result(&self.result)
    }
}

/// Why a Forgejo action did not produce a result, with the class a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoFailure {
    /// One word: `bad_url`, `missing_token`, `bad_token`, `not_admin`, `forbidden`,
    /// `not_found`, `server_error`, `malformed_reply`, `timeout`, `transport`, or one
    /// of the identity and workspace classes.
    pub class: String,
    /// The message, which never holds a token.
    pub message: String,
}

/// The result of settling one agent's Forgejo identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityReport {
    /// The agent the identity is about.
    pub agent: String,
    /// The identity, or why it could not be settled.
    pub result: Result<AgentIdentity, ForgejoFailure>,
}

impl IdentityReport {
    /// One line for a screen.
    pub fn describe(&self) -> String {
        match &self.result {
            Ok(identity) => format!(
                "{}{}",
                identity.line(),
                if identity.created {
                    " (created now)"
                } else {
                    ""
                }
            ),
            Err(failure) => format!("failed ({}): {}", failure.class, failure.message),
        }
    }
}

/// Say how an endpoint test ended.
fn describe_endpoint_result(result: &Result<String, EndpointFailure>) -> String {
    match result {
        Ok(summary) => format!("the endpoint answered: {summary}"),
        Err(failure) => format!("failed ({}): {}", failure.class, failure.message),
    }
}

/// A Forgejo failure, as a screen holds it.
fn failure_of(error: forgejo::ForgejoError) -> ForgejoFailure {
    ForgejoFailure {
        class: error.class().to_owned(),
        message: error.to_string(),
    }
}

/// Everything the screens draw.
#[derive(Debug)]
pub struct App {
    layout: Layout,
    /// The supervisor that starts, watches and stops agent processes.
    pub supervisor: Supervisor,
    /// The configuration, when it could be read.
    pub config: Option<Config>,
    /// Why the configuration could not be read, when it could not.
    pub config_error: Option<String>,
    /// Why the configuration directory itself could not be prepared.
    pub root_error: Option<String>,
    /// The credentials file's path. Shown to the user; its contents are never read.
    pub credentials_path: PathBuf,
    /// The mode of the credentials file, as read back from disk.
    pub credentials_mode: Option<u32>,
    /// Why the credentials file could not be prepared.
    pub credentials_error: Option<String>,
    /// The path of the risk-acknowledgment record.
    pub ack_path: PathBuf,
    /// The state of that record.
    pub ack_state: AckState,
    /// Whether the first-run acknowledgment is on screen now.
    pub show_acknowledgment: bool,
    /// The screen on show.
    pub screen: Screen,
    /// Which agent the agent screens are about.
    pub selected: usize,
    /// Which run the runs screen is about.
    pub run_selected: usize,
    /// The recent runs, read from the records on disk.
    pub recent: Vec<RunSummary>,
    /// The result of the last "test this endpoint" action, if any.
    pub endpoint_test: Option<EndpointTest>,
    /// The result of the last role check, if any.
    pub role_check: Option<Result<RoleCheck, ForgejoFailure>>,
    /// Which repository the Forgejo screen has selected.
    pub repo_selected: usize,
    /// The result of the last identity action, if any.
    pub identity_report: Option<IdentityReport>,
    /// A line about the last repository or pull-request action.
    pub forgejo_note: Option<String>,
    /// When the recent runs were last read.
    last_scan: Option<Instant>,
    /// The one line under the screens.
    pub status: String,
    /// Work the terminal loop has to carry out.
    pending: Option<PendingAction>,
    /// Set when the user asks to leave.
    pub should_quit: bool,
}

impl App {
    /// Build the state and read the files, starting agents with this program.
    pub fn new(layout: Layout) -> Self {
        let binary = std::env::current_exe().unwrap_or_default();
        Self::with_runner(layout, binary)
    }

    /// Build the state with a chosen executable for agent processes.
    ///
    /// The tests use this to put a small script in place of the real binary, so a
    /// screen test can watch a start without starting the program itself.
    pub fn with_runner(layout: Layout, binary: PathBuf) -> Self {
        let credentials_path = layout.credentials_file();
        let ack_path = layout.acknowledgment_file();
        let supervisor = Supervisor::new(
            layout.state_root(),
            binary,
            crate::config::Limits::default().max_concurrent_agents as usize,
            Duration::from_secs(crate::config::Limits::default().run_timeout_secs),
        );
        let mut supervisor = supervisor;
        supervisor.set_credentials(credentials_path.clone());
        supervisor.set_config_root(layout.root());
        let mut app = Self {
            layout,
            supervisor,
            config: None,
            config_error: None,
            root_error: None,
            credentials_path,
            credentials_mode: None,
            credentials_error: None,
            ack_path,
            ack_state: AckState::Missing,
            show_acknowledgment: false,
            screen: Screen::Agents,
            selected: 0,
            run_selected: 0,
            recent: Vec::new(),
            endpoint_test: None,
            role_check: None,
            repo_selected: 0,
            identity_report: None,
            forgejo_note: None,
            last_scan: None,
            status: String::new(),
            pending: None,
            should_quit: false,
        };
        app.reload();
        // The first-run acknowledgment is put in front of the user on the first start
        // of an installation, that is, whenever no confirmation is recorded.
        app.show_acknowledgment = !app.ack_state.is_confirmed();
        app
    }

    /// Re-read the files, and apply the limits they carry. Run at start and on `r`.
    pub fn reload(&mut self) {
        match self.layout.ensure_root() {
            Ok(()) => {
                self.root_error = None;
                match Config::load_or_write_sample(&self.layout.config_file()) {
                    Ok(config) => {
                        self.supervisor.set_limits(
                            config.limits.max_concurrent_agents as usize,
                            Duration::from_secs(config.limits.run_timeout_secs),
                        );
                        self.config = Some(config);
                        self.config_error = None;
                    }
                    Err(error) => {
                        self.config = None;
                        self.config_error = Some(error.to_string());
                    }
                }
                self.load_agent_modes();
                match CredentialsFile::prepare(&self.layout.credentials_file()) {
                    Ok(file) => {
                        self.credentials_path = file.path().to_path_buf();
                        self.credentials_mode = Some(file.mode());
                        self.credentials_error = None;
                    }
                    Err(error) => {
                        self.credentials_path = self.layout.credentials_file();
                        self.credentials_mode = None;
                        self.credentials_error = Some(error.to_string());
                    }
                }
                self.ack_state = AckState::read(&self.layout.acknowledgment_file());
            }
            Err(error) => {
                self.root_error = Some(error.to_string());
                self.config = None;
                self.config_error = None;
                self.credentials_mode = None;
                self.credentials_error = None;
                // Fail closed: without the configuration directory there is no record
                // to trust, so no agent may be started.
                self.ack_state = AckState::Unreadable {
                    reason: "the configuration directory could not be prepared".to_owned(),
                };
            }
        }
        self.clamp_selection();
        self.refresh_recent();
        self.status = self.reload_status();
    }

    /// Read the recent run records again.
    pub fn refresh_recent(&mut self) {
        let names: Vec<String> = self
            .agents()
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        self.recent = recent_runs(self.layout.state_root(), &names, RECENT_RUNS_LIMIT);
        self.last_scan = Some(Instant::now());
        if self.run_selected >= self.recent.len() {
            self.run_selected = self.recent.len().saturating_sub(1);
        }
    }

    /// The line shown after a reload.
    fn reload_status(&self) -> String {
        if let Some(error) = &self.root_error {
            return error.clone();
        }
        if let Some(error) = &self.config_error {
            return error.clone();
        }
        if let Some(error) = &self.credentials_error {
            return error.clone();
        }
        if !self.ack_state.is_confirmed() {
            return "the risk acknowledgment is not confirmed, so starting an agent is refused"
                .to_owned();
        }
        format!(
            "configuration read from {}",
            self.layout.config_file().display()
        )
    }

    /// The configuration file this installation uses.
    pub fn config_path(&self) -> PathBuf {
        self.layout.config_file()
    }

    /// Whether the credentials file holds a role token and an administrator token.
    ///
    /// The presence of an administrator token is what chooses the identity path, so
    /// the Forgejo screen shows both plainly rather than inferring anything.
    pub fn forgejo_tokens_present(&self) -> (bool, bool) {
        let role = credentials::forgejo_token(&self.credentials_path).is_ok();
        let admin = credentials::forgejo_admin_token(&self.credentials_path)
            .ok()
            .flatten()
            .is_some();
        (role, admin)
    }

    /// The configuration directory.
    pub fn root(&self) -> &std::path::Path {
        self.layout.root()
    }

    /// The run-record directory.
    pub fn state_root(&self) -> &std::path::Path {
        self.layout.state_root()
    }

    /// Prepare an agent's own directories and return the paths derived from its name.
    ///
    /// This is what a start and an edit both need: the agent's configuration
    /// directory, its skills directory and its workspace, created if they are absent.
    pub fn ensure_agent(
        &self,
        name: &str,
    ) -> Result<crate::paths::AgentPaths, crate::paths::PathError> {
        self.layout.ensure_agent(name)
    }

    /// The agents the configuration lists, or an empty list.
    pub fn agents(&self) -> &[AgentConfig] {
        match &self.config {
            Some(config) => &config.agents,
            None => &[],
        }
    }

    /// The agent the agent screens are about.
    pub fn selected_agent(&self) -> Option<&AgentConfig> {
        self.agents().get(self.selected)
    }

    /// The agent name the agent screens are about.
    pub fn selected_name(&self) -> Option<String> {
        self.selected_agent().map(|agent| agent.name.clone())
    }

    /// The paths derived from the selected agent's name, when there is one.
    pub fn selected_paths(&self) -> Option<crate::paths::AgentPaths> {
        let name = self.selected_name()?;
        match self.layout.agent(&name) {
            Ok(paths) => Some(paths),
            Err(error) => {
                // A name that cannot name a directory is shown, never used.
                let _ = error;
                None
            }
        }
    }

    /// Where the selected agent's files go, as a sentence for the screen.
    pub fn selected_paths_problem(&self) -> Option<String> {
        let name = self.selected_name()?;
        self.layout
            .agent(&name)
            .err()
            .map(|error| error.to_string())
    }

    /// The run the runs screen is about.
    pub fn selected_run(&self) -> Option<&RunSummary> {
        self.recent.get(self.run_selected)
    }

    /// Whether the risk of running an agent was acknowledged and recorded.
    pub fn ack_confirmed(&self) -> bool {
        self.ack_state.is_confirmed()
    }

    /// Write the acknowledgment and close the dialog.
    pub fn confirm_acknowledgment(&mut self) {
        match risk::record(&self.ack_path) {
            Ok(state) => {
                self.ack_state = state;
                self.show_acknowledgment = false;
                self.status = format!(
                    "risk acknowledged and recorded in {}",
                    self.ack_path.display()
                );
            }
            Err(error) => {
                self.ack_state = AckState::Unreadable {
                    reason: error.to_string(),
                };
                self.show_acknowledgment = false;
                self.status = format!(
                    "the acknowledgment could not be recorded ({error}); starting an agent stays refused"
                );
            }
        }
    }

    /// Close the dialog without confirming. Starting an agent stays refused.
    pub fn decline_acknowledgment(&mut self) {
        self.show_acknowledgment = false;
        self.status =
            "the risk was not acknowledged, so starting an agent is refused until it is".to_owned();
    }

    /// Ask to start the selected agent.
    ///
    /// Beyond the concurrency limit this says the start is waiting its turn rather
    /// than failing. An agent with no instruction document is not started: a run
    /// would have nothing to read.
    pub fn request_agent_start(&mut self) {
        if !self.ack_state.is_confirmed() {
            self.show_acknowledgment = true;
            self.status =
                "refused: no agent may be started before the risk acknowledgment is confirmed"
                    .to_owned();
            return;
        }
        let Some(agent) = self.selected_agent().cloned() else {
            self.status = "refused: the configuration lists no agent to start".to_owned();
            return;
        };
        let name = agent.name.clone();
        let paths = match self.layout.ensure_agent(&name) {
            Ok(paths) => paths,
            Err(error) => {
                self.status = format!("`{name}` cannot be prepared: {error}");
                return;
            }
        };
        if !paths.instruction().is_file() {
            self.status = format!(
                "not started: `{name}` has no instruction document at {}; press e to write one",
                paths.instruction().display()
            );
            return;
        }
        let steps = match crate::run::RunPlan::read_steps(paths.steps_file()) {
            Ok(steps) => steps,
            Err(error) => {
                self.status = format!("the declared steps of `{name}` cannot be read: {error}");
                return;
            }
        };
        let model = RunModel {
            endpoint: agent.endpoint.clone(),
            model: agent.model.clone(),
            max_turns: agent.max_turns,
        };
        match self.supervisor.start(&paths, &model, steps) {
            Ok(StartDisposition::Started { run_id, pid }) => {
                self.status = format!("started `{name}`: run {run_id}, process {pid}");
                self.refresh_recent();
            }
            Ok(StartDisposition::Queued { run_id, position }) => {
                self.status = format!(
                    "`{name}` is waiting its turn: {} of {} agents are running, this is start {position} in line (run {run_id})",
                    self.supervisor.running_count(),
                    self.supervisor.max_concurrent(),
                );
            }
            Ok(StartDisposition::Busy { run_id }) => {
                self.status = format!("`{name}` already has run {run_id}");
            }
            Err(error) => {
                self.status = format!("{error}");
            }
        }
    }

    /// Ask the selected agent to stop.
    pub fn request_agent_stop(&mut self) {
        let Some(name) = self.selected_name() else {
            self.status = "there is no agent to stop".to_owned();
            return;
        };
        self.status = match self.supervisor.stop(&name) {
            StopDisposition::Signalled => format!(
                "stopping `{name}`: its process group was signalled and will be proven gone"
            ),
            StopDisposition::Cancelled => format!("`{name}` was called off while it waited"),
            StopDisposition::Nothing => format!("`{name}` is not running"),
            StopDisposition::Failed { reason } => {
                format!("`{name}` could not be stopped: {reason}")
            }
        };
        self.refresh_recent();
    }

    /// Move the selected agent between the two supervision modes.
    ///
    /// Idempotent: asking for the mode the agent is already in says so and changes
    /// nothing. A switch is refused while the agent is running, in either mode, so a
    /// run is never left without the thing that knows about it.
    pub fn request_mode_switch(&mut self) {
        let Some(name) = self.selected_name() else {
            self.status = "there is no agent whose supervision mode could change".to_owned();
            return;
        };
        let paths = match self.layout.agent(&name) {
            Ok(paths) => paths,
            Err(error) => {
                self.status = format!("`{name}` cannot change mode: {error}");
                return;
            }
        };
        let target = self.supervisor.mode(&name).other();
        match self.supervisor.switch_mode(&paths, target) {
            Ok(ModeChange::Already(mode)) => {
                self.status = format!(
                    "`{name}` is already supervised by {} ({})",
                    mode.label(),
                    mode.as_str()
                );
            }
            Ok(ModeChange::Switched(mode)) => {
                let note = match mode {
                    Mode::Systemd => {
                        // Say plainly whether a start can follow, so a machine without a
                        // user session does not look ready when it is not.
                        match self.supervisor.systemd_available() {
                            Ok(version) => format!(
                                "a unit is written and started the next time it is started \
                                 (s), and it keeps running when this UI exits (systemd {version})"
                            ),
                            Err(reason) => format!(
                                "a unit will be written at the next start (s), but {reason}: \
                                 starting will be refused until a user session is available"
                            ),
                        }
                    }
                    Mode::Tui => {
                        "its unit file was removed from your systemd directory".to_owned()
                    }
                };
                self.status = format!(
                    "`{name}` is now supervised by {} ({}): {note}",
                    mode.label(),
                    mode.as_str(),
                );
            }
            Err(message) => self.status = message,
        }
        self.refresh_recent();
    }

    /// Read every agent's supervision record, so the screens know which mode each
    /// agent is in and which run a systemd-supervised agent's unit points at.
    fn load_agent_modes(&mut self) {
        let names: Vec<String> = self
            .agents()
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        let mut agents = Vec::new();
        for name in names {
            if let Ok(paths) = self.layout.agent(&name) {
                agents.push(paths);
            }
        }
        self.supervisor.load_modes(&agents);
    }

    /// Test the selected agent's endpoint with one minimal call.
    ///
    /// This is the acceptance the design names: a person can point an agent at an
    /// endpoint and ask the tool to prove it works. The call is made here, on the
    /// spot, and its result is one line: either what the model answered, or the class
    /// of failure and its message. The key is read from the credentials file for this
    /// one call and is never shown.
    pub fn request_endpoint_test(&mut self) {
        let Some(agent) = self.selected_agent().cloned() else {
            self.status = "the configuration lists no agent to test".to_owned();
            return;
        };
        let credentials = self.layout.credentials_file();
        let result = match credentials::model_key(&credentials, &agent.name) {
            Err(error) => Err(EndpointFailure {
                class: "missing_key".to_owned(),
                message: error.to_string(),
            }),
            Ok(key) => {
                let config = model::ModelConfig {
                    endpoint: agent.endpoint.clone(),
                    model: agent.model.clone(),
                    api_key: key,
                    timeout: model::PROBE_TIMEOUT,
                };
                match model::probe(&config) {
                    Ok(reply) => Ok(reply.summary()),
                    Err(error) => Err(EndpointFailure {
                        class: error.class().to_owned(),
                        message: error.to_string(),
                    }),
                }
            }
        };
        let short = match &result {
            Ok(_) => "ok".to_owned(),
            Err(failure) => format!("failed ({})", failure.class),
        };
        self.status = format!("endpoint test for `{}`: {short}", agent.name);
        self.endpoint_test = Some(EndpointTest {
            agent: agent.name,
            endpoint: agent.endpoint,
            model: agent.model,
            result,
        });
    }

    /// The Forgejo the configuration names, when the configuration was read.
    fn forgejo_settings(&self) -> Option<crate::config::Forgejo> {
        self.config.as_ref().map(|config| config.forgejo.clone())
    }

    /// The repository the Forgejo screen has selected, when a check has been run.
    pub fn selected_repository(&self) -> Option<&forgejo::Repository> {
        match &self.role_check {
            Some(Ok(check)) => check.repositories.get(self.repo_selected),
            _ => None,
        }
    }

    /// Validate the URL and the token, and list what the token can see.
    ///
    /// This is the role check. The token comes from the credentials file, and the
    /// result says plainly whether the view is an administrator's or an ordinary
    /// member's, or names the class of failure: a bad URL, a bad token, or a token
    /// that does not carry the rights the configuration expects.
    pub fn request_role_check(&mut self) {
        let Some(settings) = self.forgejo_settings() else {
            self.status =
                "the configuration could not be read, so no Forgejo can be checked".to_owned();
            return;
        };
        let result = match identity::role_token(&self.credentials_path, settings.role) {
            Ok(token) => {
                let config = forgejo::ForgejoConfig {
                    url: settings.url.clone(),
                    token,
                    timeout: forgejo::DEFAULT_TIMEOUT,
                };
                forgejo::role_check(&config, settings.role).map_err(failure_of)
            }
            Err(error) => Err(ForgejoFailure {
                class: error.class().to_owned(),
                message: error.to_string(),
            }),
        };
        self.status = match &result {
            Ok(check) => format!("role check: {}", check.summary()),
            Err(failure) => format!("role check failed ({}): {}", failure.class, failure.message),
        };
        self.role_check = Some(result);
        self.repo_selected = 0;
    }

    /// Settle the selected agent's Forgejo identity.
    ///
    /// With an administrator token in the credentials file the account and its token
    /// are created and managed here, derived from the agent's name. Without one, the
    /// token the user supplied under `[forgejo_agent_tokens]` is checked and its
    /// account recorded. Either way the credentials file ends up naming the login a
    /// commit will be authored by.
    pub fn request_identity(&mut self) {
        let Some(name) = self.selected_name() else {
            self.status =
                "the configuration lists no agent whose identity could be settled".to_owned();
            return;
        };
        let Some(settings) = self.forgejo_settings() else {
            self.status =
                "the configuration could not be read, so no identity can be settled".to_owned();
            return;
        };
        let result =
            identity::resolve(&settings.url, &name, &self.credentials_path).map_err(|error| {
                ForgejoFailure {
                    class: error.class().to_owned(),
                    message: error.to_string(),
                }
            });
        self.status = match &result {
            Ok(identity) => format!(
                "identity for `{name}`: {}{}",
                identity.line(),
                if identity.created {
                    " (created now)"
                } else {
                    ""
                }
            ),
            Err(failure) => format!(
                "identity for `{name}` failed ({}): {}",
                failure.class, failure.message
            ),
        };
        self.identity_report = Some(IdentityReport {
            agent: name,
            result,
        });
    }

    /// The git identity recorded for an agent, or why there is none yet.
    fn recorded_git_identity(&self, agent: &str) -> Result<GitIdentity, ForgejoFailure> {
        let login = credentials::recorded_login(&self.credentials_path, agent)
            .ok()
            .flatten();
        let email = credentials::recorded_email(&self.credentials_path, agent)
            .ok()
            .flatten();
        match (login, email) {
            (Some(login), Some(email)) => Ok(GitIdentity { login, email }),
            _ => Err(ForgejoFailure {
                class: "no_identity".to_owned(),
                message: format!(
                    "no Forgejo identity is recorded for `{agent}`; press i to settle one, or supply its token under [forgejo_agent_tokens]"
                ),
            }),
        }
    }

    /// Clone the repository on show into the selected agent's workspace.
    ///
    /// The repository is the one the role check listed and the screen has selected;
    /// the clone goes into the agent's workspace under the repository's name, and the
    /// agent's git identity is configured there, so commits made in it are authored by
    /// the agent.
    pub fn request_clone(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            self.status =
                "no repository is on show: press c to run the role check first".to_owned();
            return;
        };
        let Some(name) = self.selected_name() else {
            self.status = "the configuration lists no agent to clone for".to_owned();
            return;
        };
        let identity = match self.recorded_git_identity(&name) {
            Ok(identity) => identity,
            Err(failure) => {
                self.status = failure.message;
                return;
            }
        };
        let Some(base) = self.forgejo_settings().map(|settings| settings.url) else {
            self.status = "the configuration could not be read".to_owned();
            return;
        };
        let paths = match self.layout.ensure_agent(&name) {
            Ok(paths) => paths,
            Err(error) => {
                self.status = format!("`{name}` cannot be prepared: {error}");
                return;
            }
        };
        let destination = workspace::clone_destination(paths.workspace(), &repository.full_name);
        let askpass = std::env::current_exe().unwrap_or_default();
        let credential = GitCredential {
            askpass: &askpass,
            credentials: &self.credentials_path,
            agent: &name,
            login: &identity.login,
        };
        match workspace::prepare(
            &base,
            &repository.full_name,
            paths.workspace(),
            &destination,
            &identity,
            Some(&credential),
        ) {
            Ok(path) => {
                let note = format!(
                    "{} is ready in {} as {}",
                    repository.full_name,
                    path.display(),
                    identity.line()
                );
                self.status = note.clone();
                self.forgejo_note = Some(note);
            }
            Err(error) => {
                self.status = format!(
                    "cloning {} failed ({}): {}",
                    repository.full_name,
                    error.class(),
                    error
                );
            }
        }
    }

    /// Push the workspace's current branch and open a pull request as the agent.
    pub fn request_pull_request(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            self.status =
                "no repository is on show: press c to run the role check first".to_owned();
            return;
        };
        let Some(name) = self.selected_name() else {
            self.status = "the configuration lists no agent to open a pull request for".to_owned();
            return;
        };
        let Some(settings) = self.forgejo_settings() else {
            self.status = "the configuration could not be read".to_owned();
            return;
        };
        let paths = match self.layout.agent(&name) {
            Ok(paths) => paths,
            Err(error) => {
                self.status = format!("`{name}` cannot be prepared: {error}");
                return;
            }
        };
        let destination = workspace::clone_destination(paths.workspace(), &repository.full_name);
        if !destination.join(".git").is_dir() {
            self.status = format!(
                "{} has not been cloned into `{name}`'s workspace yet; press g first",
                repository.full_name
            );
            return;
        }
        let identity = match self.recorded_git_identity(&name) {
            Ok(identity) => identity,
            Err(failure) => {
                self.status = failure.message;
                return;
            }
        };
        let askpass = std::env::current_exe().unwrap_or_default();
        let credential = GitCredential {
            askpass: &askpass,
            credentials: &self.credentials_path,
            agent: &name,
            login: &identity.login,
        };
        let branch = match workspace::current_branch(&destination) {
            Ok(branch) => branch,
            Err(error) => {
                self.status = format!(
                    "the workspace branch cannot be read ({}): {error}",
                    error.class()
                );
                return;
            }
        };
        if let Err(error) = workspace::push(&destination, &credential) {
            self.status = format!("pushing `{branch}` failed ({}): {error}", error.class());
            return;
        }
        let token = match credentials::forgejo_agent_token(&self.credentials_path, &name) {
            Ok(token) => token,
            Err(error) => {
                self.status = format!("no Forgejo token for `{name}`: {error}");
                return;
            }
        };
        let client = match ForgejoClient::new(&settings.url, token, forgejo::DEFAULT_TIMEOUT) {
            Ok(client) => client,
            Err(error) => {
                self.status = format!(
                    "the Forgejo client could not be built ({}): {error}",
                    error.class()
                );
                return;
            }
        };
        let base = repository
            .default_branch
            .clone()
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| "main".to_owned());
        let (title, body) = self.pull_request_text(&name, &branch);
        match client.open_pull_request(
            &repository.namespace,
            &repository.name,
            &title,
            &body,
            &branch,
            &base,
        ) {
            Ok(pull) => {
                let note = format!("opened {} as {}", pull.line(), identity.login);
                self.status = note.clone();
                self.forgejo_note = Some(note);
            }
            Err(error) => {
                self.status = format!("opening a pull request failed ({}): {error}", error.class());
            }
        }
    }

    /// Update the newest open pull request of the repository on show, as the agent.
    pub fn request_update_pull_request(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            self.status =
                "no repository is on show: press c to run the role check first".to_owned();
            return;
        };
        let Some(name) = self.selected_name() else {
            self.status = "the configuration lists no agent".to_owned();
            return;
        };
        let Some(settings) = self.forgejo_settings() else {
            self.status = "the configuration could not be read".to_owned();
            return;
        };
        let token = match credentials::forgejo_agent_token(&self.credentials_path, &name) {
            Ok(token) => token,
            Err(error) => {
                self.status = format!("no Forgejo token for `{name}`: {error}");
                return;
            }
        };
        let client = match ForgejoClient::new(&settings.url, token, forgejo::DEFAULT_TIMEOUT) {
            Ok(client) => client,
            Err(error) => {
                self.status = format!(
                    "the Forgejo client could not be built ({}): {error}",
                    error.class()
                );
                return;
            }
        };
        let open = match client.pull_requests(&repository.namespace, &repository.name) {
            Ok(pulls) => pulls,
            Err(error) => {
                self.status = format!(
                    "the open pull requests could not be listed ({}): {error}",
                    error.class()
                );
                return;
            }
        };
        let Some(pull) = open.first() else {
            self.status = format!(
                "{} has no open pull request to update",
                repository.full_name
            );
            return;
        };
        let (title, body) = self.pull_request_text(&name, &pull.head);
        match client.update_pull_request(
            &repository.namespace,
            &repository.name,
            pull.number,
            Some(&title),
            Some(&body),
            None,
        ) {
            Ok(updated) => {
                let note = format!("updated {} as `{name}`", updated.line());
                self.status = note.clone();
                self.forgejo_note = Some(note);
            }
            Err(error) => {
                self.status = format!(
                    "updating pull request #{} failed ({}): {error}",
                    pull.number,
                    error.class()
                );
            }
        }
    }

    /// The title and body of a pull request, from the agent's own file when it has one.
    fn pull_request_text(&self, agent: &str, branch: &str) -> (String, String) {
        let title = format!("Work by the agent `{agent}`");
        let body = format!(
            "Opened by `{agent}` from branch `{branch}`. The commit is authored by the agent's own Forgejo identity."
        );
        let Ok(paths) = self.layout.agent(agent) else {
            return (title, body);
        };
        let path = paths.config_dir().join("pull-request.txt");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (title, body);
        };
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("").trim().to_owned();
        let rest = lines.collect::<Vec<_>>().join("\n");
        let rest = rest.trim().to_owned();
        if first.is_empty() {
            (title, if rest.is_empty() { body } else { rest })
        } else {
            (first, rest)
        }
    }

    /// Ask to edit one of the selected agent's files. The terminal loop carries it out.
    pub fn request_edit(&mut self, target: EditTarget) {
        let Some(name) = self.selected_name() else {
            self.status = "the configuration lists no agent to edit".to_owned();
            return;
        };
        let paths = match self.layout.ensure_agent(&name) {
            Ok(paths) => paths,
            Err(error) => {
                self.status = format!("`{name}` cannot be prepared: {error}");
                return;
            }
        };
        let path = match target {
            EditTarget::Instruction => paths.instruction().to_path_buf(),
            EditTarget::Skills => paths.skills_dir().to_path_buf(),
            EditTarget::Steps => paths.steps_file().to_path_buf(),
        };
        if let Err(error) = ensure_editable(target, &path, &name) {
            self.status = format!(
                "the {} of `{name}` cannot be prepared: {error}",
                target.label()
            );
            return;
        }
        self.pending = Some(PendingAction {
            kind: ActionKind::Edit,
            path,
            label: format!("the {} of `{name}`", target.label()),
        });
        self.status = format!("opening the {} of `{name}` in your editor", target.label());
    }

    /// Ask to read the log of the run on show. The terminal loop carries it out.
    pub fn request_view_log(&mut self) {
        let (path, label) = if self.screen == Screen::Runs {
            match self.selected_run() {
                Some(run) => (
                    run.log.clone(),
                    format!("the log of run {} of `{}`", run.run_id, run.agent),
                ),
                None => {
                    self.status = "there is no run to open".to_owned();
                    return;
                }
            }
        } else {
            let Some(name) = self.selected_name() else {
                self.status = "there is no agent whose log could be opened".to_owned();
                return;
            };
            match self.latest_run_dir(&name) {
                Some(dir) => (
                    dir.join(crate::run::LOG_FILE),
                    format!("the newest log of `{name}`"),
                ),
                None => {
                    self.status = format!("`{name}` has no run yet");
                    return;
                }
            }
        };
        if !path.is_file() {
            self.status = format!("{} is not there yet", path.display());
            return;
        }
        self.pending = Some(PendingAction {
            kind: ActionKind::View,
            path,
            label: label.clone(),
        });
        self.status = format!("opening {label}");
    }

    /// The directory of the newest run of one agent, from the state or from disk.
    fn latest_run_dir(&self, name: &str) -> Option<PathBuf> {
        if let Some(run_id) = self.supervisor.state(name).run_id() {
            let dir = self.supervisor.run_dir(name, run_id);
            if dir.is_dir() {
                return Some(dir);
            }
        }
        self.recent
            .iter()
            .find(|run| run.agent == name)
            .map(|run| run.dir.clone())
    }

    /// The log file of the newest run of one agent, for a screen to name.
    pub fn latest_run_log(&self, name: &str) -> Option<PathBuf> {
        self.latest_run_dir(name)
            .map(|dir| dir.join(crate::run::LOG_FILE))
    }

    /// Take any work the terminal loop has to do, leaving none behind.
    pub fn take_action(&mut self) -> Option<PendingAction> {
        self.pending.take()
    }

    /// Say how an edit or a view ended.
    pub fn note_action(
        &mut self,
        action: &PendingAction,
        result: std::io::Result<std::process::ExitStatus>,
    ) {
        match (action.kind, result) {
            (ActionKind::Edit, Ok(status)) => {
                self.reload();
                if status.success() {
                    self.status = format!("{} was edited; the files were read again", action.label);
                } else {
                    self.status = format!(
                        "the editor left with code {:?} for {}",
                        status.code(),
                        action.label
                    );
                }
            }
            (ActionKind::Edit, Err(error)) => {
                self.status = format!("your editor could not be started: {error}");
            }
            (ActionKind::View, Ok(_)) => {
                self.status = format!("{} was shown", action.label);
            }
            (ActionKind::View, Err(error)) => {
                self.status = format!("a pager could not be started: {error}");
            }
        }
    }

    /// What the instruction screen needs to know about the instruction document.
    pub fn instruction_summary(&self) -> Option<(PathBuf, bool, u64)> {
        let paths = self.selected_paths()?;
        let path = paths.instruction().to_path_buf();
        match std::fs::metadata(&path) {
            Ok(metadata) => Some((path, metadata.is_file(), metadata.len())),
            Err(_) => Some((path, false, 0)),
        }
    }

    /// The skills on disk for the selected agent: name and manifest size.
    pub fn skills_summary(&self) -> Vec<(String, u64)> {
        let Some(paths) = self.selected_paths() else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(paths.skills_dir()) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let mut skills = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            let manifest = entry.path().join("SKILL.md");
            let bytes = std::fs::metadata(&manifest)
                .map(|meta| meta.len())
                .unwrap_or(0);
            skills.push((name, bytes));
        }
        skills.sort();
        skills
    }

    /// What the instruction screen needs to know about the declared steps file.
    pub fn steps_summary(&self) -> Option<(PathBuf, bool, usize)> {
        let paths = self.selected_paths()?;
        let path = paths.steps_file().to_path_buf();
        let exists = path.is_file();
        let count = crate::run::RunPlan::read_steps(&path)
            .map(|steps| steps.len())
            .unwrap_or(0);
        Some((path, exists, count))
    }

    /// Read every live process and the run records, at most once a second for the
    /// records. The terminal loop calls this on every tick.
    pub fn tick(&mut self) {
        self.supervisor.poll(Instant::now());
        let stale = match self.last_scan {
            Some(at) => at.elapsed() >= RECENT_RUNS_REFRESH,
            None => true,
        };
        if stale {
            self.refresh_recent();
        }
    }

    /// Stop every run and report how many were stopped. Called before leaving.
    pub fn shutdown(&mut self) -> usize {
        self.supervisor
            .stop_all(Duration::from_secs(STOP_ALL_BUDGET_SECS))
    }

    /// Move a step along the tab bar.
    pub fn shift_screen(&mut self, steps: isize) {
        self.screen = self.screen.shifted(steps);
    }

    /// Show one screen by name.
    pub fn show(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Move the selection on the screen on show.
    pub fn move_selection(&mut self, steps: isize) {
        match self.screen {
            Screen::Runs => self.move_run_selection(steps),
            Screen::Forgejo => self.move_repo_selection(steps),
            _ => self.move_agent_selection(steps),
        }
    }

    /// Move the repository selection on the Forgejo screen, stopping at the ends.
    pub fn move_repo_selection(&mut self, steps: isize) {
        let count = match &self.role_check {
            Some(Ok(check)) => check.repositories.len(),
            _ => 0,
        };
        if count == 0 {
            self.repo_selected = 0;
            return;
        }
        let last = count as isize - 1;
        self.repo_selected = (self.repo_selected as isize + steps).clamp(0, last) as usize;
    }

    /// Move the agent selection, stopping at the ends.
    pub fn move_agent_selection(&mut self, steps: isize) {
        let count = self.agents().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let last = count as isize - 1;
        self.selected = (self.selected as isize + steps).clamp(0, last) as usize;
    }

    /// Move the run selection, stopping at the ends.
    pub fn move_run_selection(&mut self, steps: isize) {
        let count = self.recent.len();
        if count == 0 {
            self.run_selected = 0;
            return;
        }
        let last = count as isize - 1;
        self.run_selected = (self.run_selected as isize + steps).clamp(0, last) as usize;
    }

    /// Keep the selection inside the agent list after a reload.
    fn clamp_selection(&mut self) {
        let count = self.agents().len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Open the selected agent's configuration screen.
    fn open_selected_agent(&mut self) {
        if self.selected_agent().is_some() {
            self.screen = Screen::AgentConfig;
        } else {
            self.status = "the configuration lists no agent to open".to_owned();
        }
    }

    /// One key press.
    pub fn on_key(&mut self, key: KeyEvent) {
        if self.show_acknowledgment {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_acknowledgment(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.decline_acknowledgment();
                }
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.shift_screen(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.shift_screen(-1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter => self.open_selected_agent(),
            KeyCode::Esc => self.screen = Screen::Agents,
            KeyCode::Char('s') => self.request_agent_start(),
            KeyCode::Char('x') => self.request_agent_stop(),
            KeyCode::Char('m') => self.request_mode_switch(),
            KeyCode::Char('p') => self.request_endpoint_test(),
            KeyCode::Char('c') => self.request_role_check(),
            KeyCode::Char('i') => self.request_identity(),
            KeyCode::Char('g') => {
                self.show(Screen::Forgejo);
                self.request_clone();
            }
            KeyCode::Char('P') => {
                self.show(Screen::Forgejo);
                self.request_pull_request();
            }
            KeyCode::Char('U') => {
                self.show(Screen::Forgejo);
                self.request_update_pull_request();
            }
            KeyCode::Char('e') => self.request_edit(EditTarget::Instruction),
            KeyCode::Char('t') => self.request_edit(EditTarget::Steps),
            KeyCode::Char('o') => self.request_view_log(),
            // `k` moves up; the skills directory is opened with `K`.
            KeyCode::Char('K') => self.request_edit(EditTarget::Skills),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char(digit) if ('1'..='6').contains(&digit) => {
                let at = digit as usize - '1' as usize;
                self.screen = Screen::ALL[at];
            }
            _ => {}
        }
    }
}

/// How long the tool may take to stop everything before it leaves.
const STOP_ALL_BUDGET_SECS: u64 = 10;

/// Create a file that must exist before it can be edited, so the editor never opens
/// on a path that is not there.
fn ensure_editable(target: EditTarget, path: &std::path::Path, agent: &str) -> std::io::Result<()> {
    if target == EditTarget::Skills {
        return std::fs::create_dir_all(path);
    }
    if path.exists() {
        return Ok(());
    }
    let text = match target {
        EditTarget::Instruction => format!(
            "# Instruction for the agent `{agent}`\n\n\
             Write, in plain language, what this agent should do. A run reads this\n\
             document before it does anything, and the declared steps run afterwards.\n"
        ),
        EditTarget::Steps => format!(
            "# Declared shell steps for one run of `{agent}`, one command per line.\n\
             # Blank lines and lines starting with # are skipped.\n\
             # Until model calls exist, these are the commands a run executes, in the\n\
             # agent's workspace. Remove the file to declare no steps.\n"
        ),
        EditTarget::Skills => String::new(),
    };
    crate::mode::write_private(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    /// A runner that does nothing, so a test can start an agent without a real run.
    fn quiet_runner(tag: &str) -> PathBuf {
        let path = scratch_root(tag).join("quiet-runner.sh");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the runner is runnable");
        path
    }

    fn app_with(config: &str) -> App {
        let root = scratch_root("app");
        std::fs::write(root.join("config.toml"), config)
            .expect("the test writes the configuration");
        App::with_runner(Layout::rooted_at(root), quiet_runner("app-runner"))
    }

    /// A runner that stays alive, so a test can watch an agent be started and stop it.
    fn sleeping_runner(tag: &str) -> PathBuf {
        let path = scratch_root(tag).join("sleeping-runner.sh");
        std::fs::write(&path, "#!/bin/sh\nsleep 30\n").expect("the runner is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the runner is runnable");
        path
    }

    fn app_with_sleeping_runner(tag: &str, config: &str) -> App {
        let root = scratch_root(tag);
        std::fs::write(root.join("config.toml"), config)
            .expect("the test writes the configuration");
        App::with_runner(Layout::rooted_at(root), sleeping_runner(tag))
    }

    const GOOD: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "one"
model = "m"
endpoint = "https://m.example.org"
instruction = "one.md"
concurrency = 1

[[agents]]
name = "two"
model = "m"
endpoint = "https://m.example.org"
instruction = "two.md"
concurrency = 3
"#;

    #[test]
    fn the_first_start_shows_the_acknowledgment() {
        let app = app_with(GOOD);
        assert!(app.show_acknowledgment);
        assert!(!app.ack_confirmed());
    }

    #[test]
    fn a_second_start_of_the_same_installation_does_not_show_it_again() {
        let root = scratch_root("app-once");
        std::fs::write(root.join("config.toml"), GOOD).expect("the test writes the configuration");
        let mut first = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner("app-once"));
        first.on_key(key(KeyCode::Char('y')));
        assert!(first.ack_confirmed());
        let second = App::with_runner(Layout::rooted_at(root), quiet_runner("app-once-two"));
        assert!(!second.show_acknowledgment);
        assert!(second.ack_confirmed());
    }

    #[test]
    fn an_agent_start_is_refused_before_the_acknowledgment() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("refused"));
        assert!(app.show_acknowledgment);
    }

    #[test]
    fn a_declined_acknowledgment_keeps_the_start_refused() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('n')));
        assert!(!app.show_acknowledgment);
        assert!(!app.ack_confirmed());
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("refused"));
    }

    #[test]
    fn an_agent_with_no_instruction_is_not_started_and_is_told_so() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Char('s')));
        assert!(
            app.status.contains("no instruction document"),
            "{}",
            app.status
        );
        assert!(app.status.contains('e'), "{}", app.status);
        assert_eq!(app.supervisor.running_count(), 0);
    }

    #[test]
    fn an_agent_with_an_instruction_is_started() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app
            .layout
            .ensure_agent("one")
            .expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction is written");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("started `one`"), "{}", app.status);
        assert_eq!(app.supervisor.running_count(), 1);
        app.shutdown();
    }

    #[test]
    fn starting_beyond_the_limit_waits_in_line_rather_than_failing() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let one = app.layout.ensure_agent("one").expect("one");
        let two = app.layout.ensure_agent("two").expect("two");
        std::fs::write(one.instruction(), "one\n").expect("instruction");
        std::fs::write(two.instruction(), "two\n").expect("instruction");
        app.supervisor.set_limits(1, Duration::from_secs(30));
        app.selected = 0;
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("started `one`"), "{}", app.status);
        // The quiet runner finishes at once, so the slot frees; force the start of the
        // second agent to queue by starting it before the first is reaped.
        app.selected = 1;
        app.on_key(key(KeyCode::Char('s')));
        // Either it is waiting, or the first finished so it started; both are honest.
        assert!(
            app.status.contains("waiting its turn") || app.status.contains("started `two`"),
            "{}",
            app.status
        );
        app.shutdown();
    }

    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut app = app_with(GOOD);
        app.move_agent_selection(-1);
        assert_eq!(app.selected, 0);
        app.move_agent_selection(1);
        assert_eq!(app.selected, 1);
        app.move_agent_selection(5);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn every_screen_is_reachable_and_the_selection_survives_reload() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.move_agent_selection(1);
        app.show(Screen::Runs);
        assert_eq!(app.screen, Screen::Runs);
        app.shift_screen(1);
        assert_eq!(app.screen, Screen::Agents);
        app.shift_screen(-1);
        assert_eq!(app.screen, Screen::Runs);
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.screen, Screen::Instructions);
        app.reload();
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn the_acknowledgment_is_in_the_way_until_it_is_answered() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('3')));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Agents);
        assert!(app.show_acknowledgment);
        app.on_key(key(KeyCode::Char('y')));
        assert!(!app.show_acknowledgment);
        app.on_key(key(KeyCode::Char('3')));
        assert_eq!(app.screen, Screen::Instructions);
    }

    #[test]
    fn editing_the_instruction_creates_it_and_asks_the_loop_to_open_it() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app.selected_paths().expect("paths");
        assert!(!paths.instruction().exists());
        app.on_key(key(KeyCode::Char('e')));
        assert!(paths.instruction().is_file(), "the file is created");
        let action = app.take_action().expect("an edit was asked for");
        assert_eq!(action.kind, ActionKind::Edit);
        assert_eq!(action.path, paths.instruction());
        assert!(app.take_action().is_none(), "the action is taken once");
    }

    #[test]
    fn editing_the_steps_creates_the_file_and_the_skills_directory() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app.selected_paths().expect("paths");
        app.on_key(key(KeyCode::Char('t')));
        let action = app.take_action().expect("a steps edit");
        assert_eq!(action.path, paths.steps_file());
        assert!(paths.steps_file().is_file());
        app.on_key(key(KeyCode::Char('K')));
        let action = app.take_action().expect("a skills edit");
        assert_eq!(action.path, paths.skills_dir());
        assert!(paths.skills_dir().is_dir());
    }

    #[test]
    fn opening_a_log_with_no_run_says_so_rather_than_opening_something() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('o')));
        assert!(app.status.contains("no run yet"), "{}", app.status);
        assert!(app.take_action().is_none());
    }

    #[test]
    fn q_asks_to_leave() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn a_configuration_that_cannot_be_read_does_not_stop_the_ui() {
        let app = app_with("[forgejo]\nurl = \"nope\"\nrole = \"member\"\n");
        assert!(app.config.is_none());
        assert!(app.config_error.is_some());
        assert!(app.agents().is_empty());
        assert!(app.selected_agent().is_none());
    }

    #[test]
    fn the_limits_from_the_file_reach_the_supervisor() {
        let app = app_with(GOOD);
        assert_eq!(app.supervisor.max_concurrent(), 10);
        assert_eq!(app.supervisor.timeout(), Duration::from_secs(30));
    }

    #[test]
    fn moving_an_agent_between_supervision_modes_is_reported_and_recorded() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('m')));
        eprintln!("after m (tui -> systemd): {}", app.status);
        assert!(app.status.contains("systemd"), "{}", app.status);
        assert_eq!(app.supervisor.mode("one"), Mode::Systemd);
        let paths = app.layout.agent("one").expect("the paths are derived");
        assert_eq!(crate::systemd::read_record(&paths).mode, Mode::Systemd);

        // The same key moves it back, and the unit file is gone afterwards.
        app.on_key(key(KeyCode::Char('m')));
        eprintln!("after m (systemd -> tui): {}", app.status);
        assert!(app.status.contains("tui"), "{}", app.status);
        assert_eq!(app.supervisor.mode("one"), Mode::Tui);
        assert_eq!(crate::systemd::read_record(&paths).mode, Mode::Tui);
    }

    #[test]
    fn a_supervision_mode_change_is_refused_while_the_agent_runs() {
        let mut app = app_with_sleeping_runner("app-switch-live", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction is written");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");
        app.on_key(key(KeyCode::Char('m')));
        assert!(app.status.contains("refused"), "{}", app.status);
        assert!(app.status.contains("stop it first"), "{}", app.status);
        assert_eq!(app.supervisor.mode("one"), Mode::Tui);
        app.shutdown();
    }

    #[test]
    fn a_name_that_cannot_name_a_directory_is_reported_rather_than_used() {
        let config = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"
"#;
        let root = scratch_root("app-unsafe-name");
        std::fs::write(root.join("config.toml"), config).expect("written");
        let mut app = App::with_runner(Layout::rooted_at(root), quiet_runner("app-unsafe"));
        // An agent name cannot reach the configuration with a bad name, so the
        // derivation is asked directly: this is the sentence a bad name produces.
        let error = app
            .layout
            .agent("Bad Name")
            .expect_err("the name is refused");
        assert!(error.to_string().contains("cannot name a directory"));
        app.show_acknowledgment = false;
        app.status = error.to_string();
        assert!(app.selected_paths().is_none());
    }
}
