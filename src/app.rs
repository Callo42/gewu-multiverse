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

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{AgentConfig, Config, ConfigError, Role};
use crate::credentials::{self, CredentialsFile};
use crate::forgejo::{self, ForgejoClient, RoleCheck};
use crate::identity::{self, AgentIdentity};
use crate::model;
use crate::paths::Layout;
use crate::risk::{self, AckState};
use crate::run::{RunModel, RunOutcome, RunSummary, recent_runs};
use crate::supervisor::{ModeChange, StartDisposition, State, StopDisposition, Supervisor};
use crate::systemd::Mode;
use crate::workspace::{self, GitCredential, GitIdentity};

/// How often the runs screen re-reads the records on disk.
const RECENT_RUNS_REFRESH: Duration = Duration::from_secs(1);

/// How many runs the runs screen lists.
const RECENT_RUNS_LIMIT: usize = 50;

/// How many rows one page key moves a list, and the help overlay.
const LIST_PAGE: isize = 5;
const HELP_PAGE: usize = 5;
/// How many logical lines `PageDown`/`PageUp` move a panel's own scroll.
const PANEL_PAGE: isize = 5;

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
            EditTarget::Skills => "new skill",
            EditTarget::Steps => "declared steps",
        }
    }
}

/// The in-TUI form that gets a first user to a named agent without a text editor.
///
/// The fields are exactly what an agent needs to run against a model, plus the
/// Forgejo connection when the user has one. The two secret fields are held here but
/// never drawn as their value: `ui` masks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentForm {
    /// name, endpoint, model, model key, Forgejo URL, Forgejo role token.
    pub fields: [String; 6],
    /// Which field the next key types into.
    pub cursor: usize,
    /// Why the last attempt to create the agent did not work.
    pub error: Option<String>,
}

impl NewAgentForm {
    /// The field labels, in order, and whether a field holds a secret.
    pub const FIELDS: [(&'static str, bool); 6] = [
        ("agent name", false),
        ("model endpoint", false),
        ("model id", false),
        ("model key", true),
        ("Forgejo URL", false),
        ("Forgejo role token", true),
    ];

    /// A blank form, ready at the first field.
    pub fn new() -> Self {
        Self {
            fields: Default::default(),
            cursor: 0,
            error: None,
        }
    }
}

impl Default for NewAgentForm {
    fn default() -> Self {
        Self::new()
    }
}

/// Which secret a one-field prompt is collecting.
///
/// The add-agent form collects the first model key and Forgejo token; this is how a
/// secret that is needed *after* the agent exists is supplied without opening the
/// credentials file in an editor. A value is never drawn: the prompt masks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretTarget {
    /// The model key for the selected agent.
    ModelKey,
    /// The token for the configured Forgejo role.
    ForgejoRoleToken,
    /// The Forgejo administrator token, used to manage identities.
    ForgejoAdminToken,
    /// The selected agent's own Forgejo token, used to settle a supplied identity.
    ForgejoAgentToken,
}

impl SecretTarget {
    /// The words the prompt and the status line use.
    pub fn describe(self) -> &'static str {
        match self {
            SecretTarget::ModelKey => "model key",
            SecretTarget::ForgejoRoleToken => "Forgejo role token",
            SecretTarget::ForgejoAdminToken => "Forgejo administrator token",
            SecretTarget::ForgejoAgentToken => "agent's Forgejo token",
        }
    }

    /// Whether the secret belongs to one agent, rather than to the connection.
    pub fn needs_agent(self) -> bool {
        matches!(
            self,
            SecretTarget::ModelKey | SecretTarget::ForgejoAgentToken
        )
    }
}

/// A one-field, masked prompt for a secret that is written to the credentials file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretPrompt {
    /// Which secret is being collected.
    pub target: SecretTarget,
    /// The agent the secret belongs to, when the secret is per-agent. Named in the
    /// prompt, because the subject is chosen on another screen and a write is not
    /// undoable (T-4).
    pub agent: Option<String>,
    /// What has been typed so far. Drawn as `*`, never as itself.
    pub value: String,
    /// Why the last attempt to record it did not work.
    pub error: Option<String>,
}

impl SecretPrompt {
    /// A blank prompt for one target.
    pub fn new(target: SecretTarget) -> Self {
        Self {
            target,
            agent: None,
            value: String::new(),
            error: None,
        }
    }

    /// The subject of the write, in the product's words, with its article.
    pub fn subject(&self) -> String {
        match &self.agent {
            Some(agent) => format!("{} for `{agent}`", self.target.describe()),
            None => self.target.describe().to_owned(),
        }
    }
}

/// A cheap fingerprint of a file: its size and its modification time.
///
/// Taken before an editor is handed a path and compared after it returns, so the
/// tool can tell an unchanged file from an edited one instead of assuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

/// The fingerprint of a path, when it can be read.
fn stamp(path: &std::path::Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// An editor's emergency save for a file, when one is there.
///
/// nano writes `<name>.save`, vim writes `.<name>.swp`. Both are the user's words,
/// kept by the editor when an interruption stopped the save; the product names the
/// file rather than pretending the edit never happened.
fn editor_backup(path: &std::path::Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let dir = path.parent()?;
    for candidate in [format!("{name}.save"), format!(".{name}.swp")] {
        let candidate = dir.join(candidate);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
    /// The file's fingerprint before the hand-off, so an unchanged file can be told
    /// from an edited one. `None` for a view, or when the file did not exist.
    pub before: Option<FileStamp>,
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
    /// When the test was run, in unix milliseconds. A result with no time can outlive
    /// the endpoint it tested and still read as current (Y-5).
    pub tested_at_ms: u64,
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
        format!(
            "{} — tested {}",
            describe_endpoint_result(&self.result),
            tested_age(self.tested_at_ms)
        )
    }

    /// Whether this result is still a claim about the agent as it is configured now.
    ///
    /// A test of a different endpoint or model is history, not the current state, and
    /// must not be shown as if it were (Y-5).
    pub fn is_current_for(&self, agent: &crate::config::AgentConfig) -> bool {
        self.agent == agent.name && self.endpoint == agent.endpoint && self.model == agent.model
    }
}

/// How long ago a test ran, in words, so a success cannot masquerade as current.
fn tested_age(tested_at_ms: u64) -> String {
    let now = crate::time::now_unix_ms();
    let seconds = now.saturating_sub(tested_at_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => {
            let minutes = seconds / 60;
            if minutes == 1 {
                "1 minute ago".to_owned()
            } else {
                format!("{minutes} minutes ago")
            }
        }
        3600..=86_399 => {
            let hours = seconds / 3600;
            if hours == 1 {
                "1 hour ago".to_owned()
            } else {
                format!("{hours} hours ago")
            }
        }
        _ => {
            let days = seconds / 86_400;
            if days == 1 {
                "1 day ago".to_owned()
            } else {
                format!("{days} days ago")
            }
        }
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

/// The status a failed clone shows.
///
/// The account the clone acted as and where the repository list came from lead the
/// sentence, before git's own detail: the detail is two lines and fills the status
/// area, so a trailing explanation is the part that wraps off the screen (W-3).
fn clone_failed_status(repo: &str, login: &str, mode: &str, class: &str, detail: &str) -> String {
    format!(
        "clone {repo} acted as {login}{mode} (the list came from the role token): failed ({class}): {detail}"
    )
}

/// Say how an endpoint test ended, in the product's own words.
fn describe_endpoint_result(result: &Result<String, EndpointFailure>) -> String {
    match result {
        Ok(summary) => format!("the endpoint answered: {summary}"),
        Err(failure) => format!(
            "failed ({}): {}",
            failure.class,
            crate::model::in_product_words(&failure.message)
        ),
    }
}

/// The agent name in a secret label like `model key for \`scribe\``, when there is one.
fn backticked_agent(label: &str) -> Option<String> {
    let start = label.find('`')? + 1;
    let end = label[start..].find('`')? + start;
    Some(label[start..end].to_owned())
}

/// A Forgejo failure, as a screen holds it.
///
/// The transport's own words are translated at the point the failure is built, so
/// every screen that shows it — the role check's status, the panel's refusal, an
/// identity attempt — reads in the product's vocabulary (O-4). A Forgejo action
/// leaves no run record to keep the raw sentence in.
fn failure_of(error: forgejo::ForgejoError) -> ForgejoFailure {
    ForgejoFailure {
        class: error.class().to_owned(),
        message: crate::model::in_product_words(&error.to_string()),
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
    /// The in-TUI add-agent form, when it is open.
    pub add_agent: Option<NewAgentForm>,
    /// A one-field prompt for a secret, when one is open.
    pub secret_prompt: Option<SecretPrompt>,
    /// An agent waiting on the "remove it?" confirmation, when one is open.
    pub confirm_remove: Option<String>,
    /// The leftover per-agent secrets (agents no longer configured) waiting on a delete
    /// confirmation, when one is open (Y-6).
    pub confirm_forget: Option<Vec<String>>,
    /// An edit waiting on the no-editor confirmation, when `$EDITOR` is unset.
    pub editor_confirm: Option<PendingAction>,
    /// Whether the user set `$EDITOR`/`$VISUAL`. Read once, so tests can vary it.
    pub editor_configured: bool,
    /// Whether the key-reference overlay is on screen now.
    pub show_help: bool,
    /// Runs in flight when the user first pressed `q`, while a quit waits to be
    /// confirmed. Any other key cancels it.
    pub confirm_quit: Option<usize>,
    /// How far the key-reference overlay has been scrolled, in logical lines.
    ///
    /// The help is longer than a short window can draw, so it is a list like any
    /// other: it scrolls with `down`/`up` and its title says the range (P-6).
    pub help_offset: usize,
    /// How far the screen's own panel or note area has been scrolled, in logical
    /// lines (Y-2). A panel that overflows can be read to its last line with `[`/`]`,
    /// and on a screen whose content is not a list the arrows and page keys move it
    /// too, so no key is swallowed in silence.
    pub panel_scroll: usize,
    /// The agent whose cached outcome was dropped on the last reload because its run
    /// record is gone (Y-4). While set, the hub says so and does not call an older
    /// record "the last run".
    pub forgotten_run: Option<String>,
    /// A line about the freshly written sample configuration, when one was written.
    pub config_note: Option<String>,
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
    /// The last start that was refused or failed, with the reason.
    pub start_failure: Option<(String, String)>,
    /// An agent whose stop was signalled and whose ending has not been read yet.
    ///
    /// The status line says `stopping` at the moment the signal is sent; when the
    /// record closes, the same agent's status is rewritten as `stopped … is proven
    /// gone`, so the row and the status never disagree about whether the stop is done.
    pending_stop: Option<String>,
    /// When this UI started, so the footer can say what finished while the user was
    /// looking at another screen.
    started_ms: u64,
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
            add_agent: None,
            secret_prompt: None,
            confirm_remove: None,
            confirm_forget: None,
            editor_confirm: None,
            editor_configured: crate::editor::editor_is_configured(),
            show_help: false,
            confirm_quit: None,
            help_offset: 0,
            panel_scroll: 0,
            forgotten_run: None,
            config_note: None,
            screen: Screen::Agents,
            selected: 0,
            run_selected: 0,
            recent: Vec::new(),
            endpoint_test: None,
            role_check: None,
            repo_selected: 0,
            identity_report: None,
            start_failure: None,
            pending_stop: None,
            started_ms: crate::time::now_unix_ms(),
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
                        self.apply_config(config);
                        self.config_note = None;
                    }
                    // The sample was just written. Load it now rather than making the
                    // user press `r` before the screen makes sense: the product itself
                    // should be usable the moment the file exists.
                    Err(ConfigError::SampleWritten { .. }) => {
                        match Config::load(&self.layout.config_file()) {
                            Ok(config) => {
                                self.apply_config(config);
                                self.config_note = Some(format!(
                                    "a commented sample configuration was written to {} — press n to add your first agent",
                                    self.layout.config_file().display()
                                ));
                            }
                            Err(error) => {
                                self.config = None;
                                self.config_error = Some(error.to_string());
                                self.config_note = None;
                            }
                        }
                    }
                    Err(error) => {
                        self.config = None;
                        self.config_error = Some(error.to_string());
                        self.config_note = None;
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
        self.reconcile_abandoned_runs();
        self.seed_states_from_records();
        self.status = self.reload_status();
        self.stop_unconfigured_runs();
    }

    /// Stop any run whose agent a re-read configuration no longer lists.
    ///
    /// `config.toml` is hand-editable. Without this, an agent removed from the file
    /// while it ran left the run counted but invisible and unstoppable — the W-2
    /// orphan shape through the reload door.
    fn stop_unconfigured_runs(&mut self) {
        let names: Vec<String> = self.agents().iter().map(|agent| agent.name.clone()).collect();
        let stopped = self.supervisor.stop_unlisted(&names);
        if !stopped.is_empty() {
            let listed = stopped.join(", ");
            // The sentence lives in the note area only: the status line drew it too,
            // so the same words appeared twice on one screen (T-10).
            self.config_note = Some(format!(
                "the configuration no longer lists {listed}; its run was stopped so that nothing runs unseen"
            ));
        }
    }

    /// Give each agent the state of its newest finished run.
    ///
    /// The supervisor only knows what it has watched in this session, so a reopened UI
    /// used to show every agent as `idle` — including agents whose newest record is
    /// `succeeded`, `stopped` or `failed`. The records are on disk; this reads the
    /// newest terminal one per agent and puts its outcome, reason and duration on the
    /// screen the user lands on. A live or already-finished state is left alone.
    fn seed_states_from_records(&mut self) {
        let names: Vec<String> = self
            .agents()
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        // This pass is the one place that can notice a record the hub had is gone.
        self.forgotten_run = None;
        for name in names {
            // An outcome whose record is gone is dropped before seeding, so `r` after
            // deleting the run records puts the hub back in step with the ledger
            // instead of leaving it claiming an outcome no record holds (T-5).
            if let State::Finished { run_id, .. } = self.supervisor.state(&name) {
                // The run's own record is the truth, on disk and now: the in-memory
                // list may predate a terminal record written by the reconciliation
                // just above. A record that is gone takes its cached outcome with it.
                let dir = self.layout.state_root().join(&name).join(&run_id);
                if crate::transcript::terminal_record(&dir).is_none() {
                    self.supervisor.forget_state(&name);
                    // Say so once, and never let the next-oldest record pass silently
                    // for "the last run" (Y-4).
                    self.forgotten_run = Some(name.clone());
                }
            }
            if self.supervisor.state(&name) != State::Idle {
                continue;
            }
            let Some(run) = self
                .recent
                .iter()
                .find(|run| run.agent == name && run.outcome.is_some())
            else {
                continue;
            };
            let Some(outcome) = run.outcome else {
                continue;
            };
            self.supervisor.note_last_outcome(
                &name,
                &run.run_id,
                outcome,
                run.reason.clone(),
                run.duration_ms.unwrap_or(0),
            );
        }
    }

    /// Close the records of default-mode runs a previous UI left behind.
    ///
    /// A `tui` run is a child of the UI that started it. When that UI is gone, nothing
    /// is left to reap the child or write its ending, and a reopened UI would call the
    /// agent `idle` while its orphan was still working — then start a second run of the
    /// same agent. The run's own opening record carries the process id, so a run whose
    /// process is no longer alive is closed here, as `stopped`, with a reason. A run
    /// whose process *is* alive is left alone: it is genuinely in progress.
    fn reconcile_abandoned_runs(&mut self) {
        let names: Vec<String> = self.agents().iter().map(|agent| agent.name.clone()).collect();
        for name in names {
            if self.supervisor.mode(&name) != Mode::Tui {
                continue;
            }
            if self.supervisor.state(&name).is_live() {
                continue;
            }
            let Some(run) = self.recent.iter().find(|run| run.agent == name) else {
                continue;
            };
            if run.outcome.is_some() {
                continue;
            }
            let Some(pid) = crate::transcript::first_started_pid(&run.dir) else {
                continue;
            };
            if crate::supervisor::process_alive(pid) {
                continue;
            }
            self.supervisor
                .note_abandoned(&name, &run.dir, &run.run_id);
        }
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

    /// Apply a freshly read configuration to the supervisor and the screens.
    fn apply_config(&mut self, config: Config) {
        self.supervisor.set_limits(
            config.limits.max_concurrent_agents as usize,
            Duration::from_secs(config.limits.run_timeout_secs),
        );
        self.config = Some(config);
        self.config_error = None;
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
        if let Some(note) = &self.config_note {
            return note.clone();
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

    /// The secret names present in the credentials file, with a note when a name
    /// belongs to no configured agent.
    ///
    /// Removing an agent rewrites `config.toml`; its model key and token stay in the
    /// credentials file, and a person auditing what exists should not see a name for
    /// an agent that does not. The secret is not deleted behind the user's back —
    /// the surface simply tells the truth about who it belongs to.
    pub fn present_secret_labels(&self) -> Vec<String> {
        let configured: Vec<&str> = self.agents().iter().map(|agent| agent.name.as_str()).collect();
        crate::credentials::present_secret_names(&self.credentials_path)
            .into_iter()
            .map(|name| match backticked_agent(&name) {
                Some(agent) if !configured.contains(&agent.as_str()) => {
                    format!("{name} (no agent with that name is configured)")
                }
                _ => name,
            })
            .collect()
    }

    /// The agents named by a secret in the credentials file that the configuration no
    /// longer lists.
    ///
    /// These are exactly the entries a removal leaves behind. Naming them is what lets
    /// the Credentials screen offer to delete them instead of only marking them (Y-6).
    pub fn leftover_secret_agents(&self) -> Vec<String> {
        let configured: Vec<&str> = self.agents().iter().map(|agent| agent.name.as_str()).collect();
        let mut agents: Vec<String> = Vec::new();
        for name in crate::credentials::present_secret_names(&self.credentials_path) {
            if let Some(agent) = backticked_agent(&name) {
                if !configured.contains(&agent.as_str()) && !agents.contains(&agent) {
                    agents.push(agent);
                }
            }
        }
        agents.sort();
        agents
    }

    /// Ask to delete the secrets left by agents that are not configured any more.
    pub fn request_forget_secrets(&mut self) {
        let agents = self.leftover_secret_agents();
        if agents.is_empty() {
            self.status =
                "every secret in the credentials file belongs to a configured agent; there is nothing to delete"
                    .to_owned();
            return;
        }
        let names = agents
            .iter()
            .map(|agent| format!("`{agent}`"))
            .collect::<Vec<_>>()
            .join(", ");
        self.status =
            format!("press y to delete the leftover secrets for {names}, or n to keep them");
        self.confirm_forget = Some(agents);
    }

    /// Delete the leftover secrets the confirmation is about.
    fn confirm_forget_secrets(&mut self) {
        let Some(agents) = self.confirm_forget.take() else {
            return;
        };
        match crate::credentials::remove_agent_secrets(&self.credentials_path, &agents) {
            Ok(removed) if removed.is_empty() => {
                self.status = "the leftover secrets were already gone".to_owned();
            }
            Ok(removed) => {
                self.status = format!(
                    "deleted from the credentials file: {} — the values were never shown",
                    removed.join(", ")
                );
            }
            Err(error) => {
                self.status = format!("the leftover secrets could not be deleted: {error}");
            }
        }
    }

    /// How many runs have finished since this UI started.
    ///
    /// The one number that answers "what happened while I was looking elsewhere".
    /// It is read from the same records the Runs screen lists: a run's end is its
    /// start time plus its recorded duration.
    pub fn runs_finished_since_start(&self) -> usize {
        self.recent
            .iter()
            .filter(|run| {
                let Some(started) = run.started_at.as_deref().and_then(crate::time::parse_rfc3339)
                else {
                    return false;
                };
                let Some(duration) = run.duration_ms else {
                    return false;
                };
                run.outcome.is_some() && started + duration > self.started_ms
            })
            .count()
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

    /// Open the add-agent form. `n` and `a` both reach it, and `enter` on an empty
    /// list reaches it too: the product itself must get a first user to an agent.
    pub fn request_new_agent(&mut self) {
        self.add_agent = Some(NewAgentForm::new());
        self.status =
            "add an agent: name it, give it an endpoint and a model, then press enter"
                .to_owned();
    }

    /// Whether the add-agent form is open.
    pub fn adding_agent(&self) -> bool {
        self.add_agent.is_some()
    }

    /// One key while the add-agent form is open.
    fn on_form_key(&mut self, key: KeyEvent) {
        let count = NewAgentForm::FIELDS.len();
        match key.code {
            KeyCode::Enter => self.submit_new_agent(),
            KeyCode::Esc => {
                self.add_agent = None;
                self.status = "no agent was added".to_owned();
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(form) = self.add_agent.as_mut() {
                    form.cursor = (form.cursor + 1) % count;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(form) = self.add_agent.as_mut() {
                    form.cursor = (form.cursor + count - 1) % count;
                }
            }
            KeyCode::Backspace => {
                if let Some(form) = self.add_agent.as_mut() {
                    form.fields[form.cursor].pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(form) = self.add_agent.as_mut() {
                    form.fields[form.cursor].push(character);
                }
            }
            _ => {}
        }
    }

    /// Try to create the agent the form describes.
    fn submit_new_agent(&mut self) {
        let Some(form) = self.add_agent.clone() else {
            return;
        };
        match self.write_new_agent(&form) {
            Ok(name) => {
                self.add_agent = None;
                self.reload();
                self.status = format!(
                    "created `{name}`: write its instruction with e, then press s to start it"
                );
            }
            Err(message) => {
                if let Some(open) = self.add_agent.as_mut() {
                    open.error = Some(message.clone());
                }
                self.status = message;
            }
        }
    }

    /// Write `config.toml` and `credentials.toml` for a new agent.
    ///
    /// The configuration is validated before it is written, so a file the product
    /// would refuse never reaches disk. A secret value is written but never returned or
    /// shown.
    fn write_new_agent(&self, form: &NewAgentForm) -> Result<String, String> {
        let name = form.fields[0].trim().to_owned();
        let endpoint = form.fields[1].trim().to_owned();
        let model = form.fields[2].trim().to_owned();
        let model_key = form.fields[3].trim().to_owned();
        let forgejo_url = form.fields[4].trim().to_owned();
        let forgejo_token = form.fields[5].trim().to_owned();
        if name.is_empty() {
            return Err("the agent needs a name".to_owned());
        }
        if let Some(problem) = crate::config::agent_name_problem(&name) {
            // The reason comes first and the echoed value is not repeated: a very long
            // name used to push the reason off the status area, leaving the user with
            // their own typing and no word of what was wrong.
            return Err(format!("cannot use that name for an agent: {problem}"));
        }
        if endpoint.is_empty() {
            return Err("the agent needs a model endpoint".to_owned());
        }
        if model.is_empty() {
            return Err("the agent needs a model id".to_owned());
        }
        // The first fix round let the form create an agent with no key, and the
        // product's own next step was then to edit `[model_keys]` in a file. Require
        // the key here: an agent the form creates can run, and no screen's only way
        // forward is a developer instruction.
        if model_key.is_empty() {
            return Err(
                "the agent needs a model key so a run can start; it is written to the \
                 credentials file and never shown"
                    .to_owned(),
            );
        }
        if self.agents().iter().any(|agent| agent.name == name) {
            return Err(format!("an agent named `{name}` already exists"));
        }
        let Some(existing) = self.config.clone() else {
            return Err(format!(
                "the configuration file cannot be read, so an agent cannot be added; fix {} first",
                self.config_path().display()
            ));
        };
        let url = if forgejo_url.is_empty() {
            existing.forgejo.url.clone()
        } else {
            forgejo_url
        };
        let text = render_config(&existing, &url, &name, &endpoint, &model);
        Config::from_text(&self.config_path(), &text).map_err(|error| error.to_string())?;
        std::fs::write(self.config_path(), text).map_err(|error| {
            format!("{} cannot be written: {error}", self.config_path().display())
        })?;
        credentials::record_form_secrets(
            &self.credentials_path,
            &name,
            (!model_key.is_empty()).then_some(model_key.as_str()),
            (!forgejo_token.is_empty()).then_some(forgejo_token.as_str()),
        )
        .map_err(|error| error.to_string())?;
        Ok(name)
    }

    /// Ask to remove the selected agent from the configuration.
    ///
    /// The first fix round made *adding* an agent a first-class in-app action;
    /// changing or removing one was still file-only, so one wrong digit meant leaving
    /// the product. Removal is the smaller half that unblocks the goal: the agent can
    /// be removed and added again. It asks first, because it is destructive, and the
    /// confirmation says what is left on disk.
    pub fn request_remove_agent(&mut self) {
        let Some(name) = self.selected_name() else {
            self.status = "there is no agent to remove".to_owned();
            return;
        };
        // Removing a running agent would leave its run behind: the record open, the
        // process alive, the footer still counting it, and no row left to select so
        // `x` cannot reach it. Refuse, as the supervision switch already does, and
        // name the run so the warning is about the thing the user would have lost.
        if let Some(run_id) = self.supervisor.unfinished_run(&name) {
            self.status = format!(
                "refused: `{name}` still has run {run_id} going; stop it first (press x), then remove"
            );
            return;
        }
        self.confirm_remove = Some(name.clone());
        self.status = format!(
            "press y to remove `{name}` from the configuration, or n to keep it"
        );
    }

    /// Remove the agent the confirmation is about, and write the configuration back.
    fn confirm_remove_agent(&mut self) {
        let Some(name) = self.confirm_remove.take() else {
            return;
        };
        let Some(existing) = self.config.clone() else {
            self.status = format!(
                "the configuration file cannot be read, so `{name}` cannot be removed"
            );
            return;
        };
        let kept: Vec<AgentConfig> = existing
            .agents
            .iter()
            .filter(|agent| agent.name != name)
            .cloned()
            .collect();
        let text = render_full_config(&existing, &kept);
        if let Err(error) = Config::from_text(&self.config_path(), &text) {
            self.status = error.to_string();
            return;
        }
        if let Err(error) = std::fs::write(self.config_path(), text) {
            self.status = format!(
                "{} cannot be written: {error}",
                self.config_path().display()
            );
            return;
        }
        let left = self
            .layout
            .agent(&name)
            .map(|paths| paths.config_dir().display().to_string())
            .unwrap_or_else(|_| "its files".to_owned());
        let had_files = self
            .layout
            .agent(&name)
            .map(|paths| paths.config_dir().is_dir())
            .unwrap_or(false);
        self.reload();
        self.status = if had_files {
            format!(
                "removed `{name}` from the configuration; its files under {left} were left on disk"
            )
        } else {
            format!(
                "removed `{name}` from the configuration; it had no files under {left} yet"
            )
        };
    }

    /// Drop the removal confirmation without removing anything.
    fn cancel_remove_agent(&mut self) {
        if let Some(name) = self.confirm_remove.take() {
            self.status = format!("`{name}` was kept");
        }
    }

    /// Open a one-field prompt for a secret the credentials file should hold.
    pub fn request_secret(&mut self, target: SecretTarget) {
        if target.needs_agent() && self.selected_name().is_none() {
            self.status = "the configuration lists no agent for that secret".to_owned();
            return;
        }
        let mut prompt = SecretPrompt::new(target);
        if target.needs_agent() {
            prompt.agent = self.selected_name();
        }
        // The prompt names its subject before the write: the agent was selected on a
        // different screen, and a secret write is not undoable (T-4).
        self.status = format!(
            "type the {} and press enter; it is written to the credentials file and never shown",
            prompt.subject()
        );
        self.secret_prompt = Some(prompt);
    }

    /// One key while the secret prompt is open.
    fn on_secret_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit_secret(),
            KeyCode::Esc => {
                self.secret_prompt = None;
                self.status = "no secret was written".to_owned();
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.secret_prompt.as_mut() {
                    prompt.value.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(prompt) = self.secret_prompt.as_mut() {
                    prompt.value.push(character);
                }
            }
            _ => {}
        }
    }

    /// Write the secret the prompt holds, and say only that it was recorded.
    fn submit_secret(&mut self) {
        let Some(prompt) = self.secret_prompt.clone() else {
            return;
        };
        let value = prompt.value.trim().to_owned();
        if value.is_empty() {
            if let Some(open) = self.secret_prompt.as_mut() {
                open.error = Some("a secret cannot be empty".to_owned());
            }
            return;
        }
        let agent = prompt.agent.clone().or_else(|| self.selected_name());
        let subject = agent.clone().unwrap_or_else(|| "the connection".to_owned());
        let (table, key) = match prompt.target {
            SecretTarget::ModelKey => (Some("model_keys"), subject.clone()),
            SecretTarget::ForgejoAgentToken => (Some("forgejo_agent_tokens"), subject.clone()),
            SecretTarget::ForgejoRoleToken => (None, "forgejo_token".to_owned()),
            SecretTarget::ForgejoAdminToken => (None, "forgejo_admin_token".to_owned()),
        };
        match credentials::record_secret(&self.credentials_path, table, &key, &value) {
            Ok(()) => {
                self.secret_prompt = None;
                self.reload();
                self.status = format!(
                    "recorded the {} for {subject}; the Credentials screen lists it by name",
                    prompt.target.describe()
                );
            }
            Err(error) => {
                if let Some(open) = self.secret_prompt.as_mut() {
                    open.error = Some(error.to_string());
                }
            }
        }
    }

    /// Ask to leave. If runs are in flight, name what leaving ends and ask again.
    pub fn request_quit(&mut self) {
        self.ask_to_leave("q");
    }

    /// Ask to leave, naming the chord that must be pressed again to go through with it.
    ///
    /// `^C` is the reflex "cancel the thing that is running" chord, and here it does
    /// what `q` does — leave and stop the runs this UI started. It therefore asks the
    /// same one question rather than leaving in silence (C-1).
    fn ask_to_leave(&mut self, chord: &str) {
        let live = self.supervisor.running_count();
        if live == 0 || self.confirm_quit.is_some() {
            self.should_quit = true;
            return;
        }
        self.confirm_quit = Some(live);
        // One run is `it`, not `them` (C-8).
        let (noun, pronoun) = if live == 1 {
            ("run", "it")
        } else {
            ("runs", "them")
        };
        self.status = format!(
            "leaving stops the {live} {noun} this UI started; press {chord} again to quit and stop {pronoun}, or any other key to stay"
        );
    }

    /// Show or hide the key-reference overlay.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
        if self.show_help {
            self.help_offset = 0;
        }
    }

    /// Why one agent's most recent run did not go well, when there is a reason.
    ///
    /// The reason is read from the run's own record, so it is there on the screen the
    /// user looks at after a failure, not only in a transient footer line.
    pub fn last_failure(&self, name: &str) -> Option<String> {
        self.newest_failed_run(name)
            .or_else(|| self.last_start_failure(name))
            .or_else(|| self.supervisor.last_error().map(str::to_owned))
    }

    /// Why one agent's most recent *failed* run did not go well, for the historical
    /// `last failure` field on the Agent screen.
    fn newest_failed_run(&self, name: &str) -> Option<String> {
        if let State::Finished {
            outcome: RunOutcome::Failed,
            reason: Some(reason),
            ..
        } = self.supervisor.state(name)
        {
            return Some(reason);
        }
        self.recent
            .iter()
            .find(|run| run.agent == name && run.outcome == Some(RunOutcome::Failed))
            .and_then(|run| run.reason.clone())
    }

    /// Why the agent's **newest** terminal run did not go well, from the run's own
    /// record.
    ///
    /// Kept apart from a start that never produced a run (D-8), and answered from the
    /// newest record, terminal or not: after a recovery the next run succeeded, and an
    /// older failure is not "the last run" (O-1). A record that is not terminal yet is
    /// not a last run either.
    pub fn last_run_failure(&self, name: &str) -> Option<String> {
        match self.supervisor.state(name) {
            State::Finished {
                outcome: RunOutcome::Failed,
                reason: Some(reason),
                ..
            } => Some(reason),
            State::Finished { .. } => None,
            _ => self
                .recent
                .iter()
                .find(|run| run.agent == name)
                .filter(|run| run.outcome == Some(RunOutcome::Failed))
                .and_then(|run| run.reason.clone()),
        }
    }

    /// Why one agent's most recent *start* did not go well, when it never became a run.
    pub fn last_start_failure(&self, name: &str) -> Option<String> {
        self.start_failure
            .as_ref()
            .filter(|(agent, _)| agent == name)
            .map(|(_, message)| message.clone())
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
        // A fresh attempt clears the previous start failure, so a stale message does
        // not sit beside a run that has since worked.
        self.start_failure = None;
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
        // Refuse before a run is wasted, with the same sentence the run's own log
        // would carry: `p` already knows the key is missing, so `s` must know too.
        if credentials::model_key(&self.credentials_path, &name).is_err() {
            // Lead with the fix; the full sentence is on the Credentials screen and in
            // the run's own log. A long path here would clip in a narrow footer.
            self.status = format!(
                "not started: no model key for `{name}` — add one under [model_keys] in credentials.toml"
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
                // Lead with the state, not the ledger: the question is "is it running
                // now?", and the answer is the word `running`, with the way to stop it.
                let state = self.supervisor.state(&name);
                self.status = if matches!(state, State::Waiting { .. }) {
                    format!(
                        "`{name}` already has a start waiting its turn (run {run_id}); press x to call it off"
                    )
                } else {
                    format!("`{name}` is already running (run {run_id}); press x to stop it")
                };
            }
            Err(error) => {
                // Keep the reason on the agent, not only in the transient status line.
                self.start_failure = Some((name.clone(), error.to_string()));
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
            StopDisposition::Signalled => {
                self.pending_stop = Some(name.clone());
                format!(
                    "stopping `{name}`: its process group was signalled and will be proven gone"
                )
            }
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
                                "a unit is written and started the next time it is started, \
                                 and it keeps running when this UI exits (systemd {version})"
                            ),
                            Err(reason) => format!(
                                "a unit will be written at the next start, but {reason}: \
                                 starting will be refused until a user session is available"
                            ),
                        }
                    }
                    Mode::Tui => {
                        "it runs as a child of this UI again; no unit file of this tool's is left in your systemd directory"
                            .to_owned()
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
            // The class alone is not the reason: the footer grows to hold the same
            // sentence the Agent screen shows, so a refused connection or a bad name
            // is read at 80×24, not just the word `transport` (O-2).
            Err(_) => describe_endpoint_result(&result),
        };
        self.status = format!("endpoint test for `{}`: {short}", agent.name);
        self.endpoint_test = Some(EndpointTest {
            agent: agent.name,
            endpoint: agent.endpoint,
            model: agent.model,
            result,
            tested_at_ms: crate::time::now_unix_ms(),
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
            // Say *not read yet* when the file is fine but has not been loaded; keep
            // "could not be read" for a real parse or access failure.
            self.status = if self.config_error.is_some() {
                "the configuration could not be read, so no Forgejo can be checked".to_owned()
            } else {
                "the configuration has not been read yet — press r to read it".to_owned()
            };
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
            // The configured URL leads the refusal: a mistyped path is the common
            // cause, and the API path alone does not connect the failure to what the
            // user typed (P-7).
            Err(failure) => format!(
                "role check failed ({}) at {}: {}",
                failure.class, settings.url, failure.message
            ),
        };
        self.role_check = Some(result);
        self.repo_selected = 0;
    }

    /// Run the role check with the administrator token, so both roles can be compared
    /// from inside the app instead of by editing the credentials file and restarting.
    pub fn request_admin_role_check(&mut self) {
        let Some(settings) = self.forgejo_settings() else {
            self.status = "the configuration could not be read, so no Forgejo can be checked"
                .to_owned();
            return;
        };
        let result = match credentials::forgejo_admin_token(&self.credentials_path) {
            Ok(Some(token)) => {
                let config = forgejo::ForgejoConfig {
                    url: settings.url.clone(),
                    token,
                    timeout: forgejo::DEFAULT_TIMEOUT,
                };
                forgejo::role_check(&config, Role::Admin).map_err(failure_of)
            }
            Ok(None) => Err(ForgejoFailure {
                class: "missing_admin_token".to_owned(),
                message: "the credentials file records no administrator token, so only the \
                          configured role can be checked"
                    .to_owned(),
            }),
            Err(error) => Err(ForgejoFailure {
                class: "credentials".to_owned(),
                message: error.to_string(),
            }),
        };
        self.status = match &result {
            Ok(check) => format!("role check (administrator token): {}", check.summary()),
            Err(failure) => format!(
                "role check with the administrator token failed ({}): {}",
                failure.class, failure.message
            ),
        };
        // A failed administrator check must not wipe a good view the user already has:
        // keep the last successful list and show the failure beside it instead.
        match result {
            Ok(check) => {
                self.role_check = Some(Ok(check));
                self.repo_selected = 0;
            }
            Err(failure) => {
                let keep = matches!(self.role_check, Some(Ok(_)));
                let message = format!(
                    "role check with the administrator token failed ({}): {}",
                    failure.class, failure.message
                );
                if keep {
                    self.forgejo_note = Some(message);
                } else {
                    self.role_check = Some(Err(failure));
                    self.repo_selected = 0;
                }
            }
        }
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
                    message: crate::model::in_product_words(&error.to_string()),
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
                // Name the account the clone acted as, and where the list came from:
                // the repository list is the role token's view, but the clone runs as
                // the agent's own identity, and the two can disagree. A bare
                // `Repository not found` leaves the user guessing which of their
                // several tokens was wrong.
                let mode = identity::recorded_mode(&self.credentials_path)
                    .ok()
                    .flatten()
                    .map(|mode| format!(" ({})", mode.as_str()))
                    .unwrap_or_default();
                // Lead with the account the clone acted as and where the list came
                // from: git's own two-line detail fills the status area, so the
                // sentence that explains *which* token was wrong must come first or
                // it wraps off the screen (W-3).
                self.status = clone_failed_status(
                    &repository.full_name,
                    &identity.login,
                    &mode,
                    error.class(),
                    &error.detail(),
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
            self.status = format!(
                "pushing `{branch}` failed ({}): {}",
                error.class(),
                error.detail()
            );
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
            // A skill is one directory with a `SKILL.md`. `K` starts a new one rather
            // than handing an editor a directory it cannot edit.
            EditTarget::Skills => paths.skills_dir().join("new-skill").join("SKILL.md"),
            EditTarget::Steps => paths.steps_file().to_path_buf(),
        };
        if let Err(error) = ensure_editable(target, &path, &name) {
            self.status = format!(
                "the {} of `{name}` cannot be prepared: {error}",
                target.label()
            );
            return;
        }
        // An interrupted editor can leave its unsaved buffer beside the file. Saying
        // so before the hand-off, and naming the file, is the difference between a
        // person recovering their words and retyping them.
        let recovery = if target == EditTarget::Instruction {
            editor_backup(&path).map(|backup| {
                format!(
                    " — an earlier edit of this instruction was not saved; the editor kept it in {} — open that file to recover",
                    backup.display()
                )
            })
        } else {
            None
        };
        let before = stamp(&path);
        let action = PendingAction {
            kind: ActionKind::Edit,
            path,
            label: format!("the {} of `{name}`", target.label()),
            before,
        };
        // A person who set no `$EDITOR` is exactly the person who does not know which
        // editor this is, or how to leave it. Name the program on screen first, and
        // wait for `enter`, because a status line would be erased the moment the UI
        // steps aside for the editor.
        let program = crate::editor::editor();
        if self.editor_configured {
            self.pending = Some(action);
            self.status = format!(
                "opening the {} of `{name}` in {program}{}",
                target.label(),
                recovery.as_deref().unwrap_or("")
            );
        } else {
            self.editor_confirm = Some(action);
            self.status = format!(
                "no $EDITOR is set: press enter to open the {} of `{name}` in {program}{}",
                target.label(),
                recovery.as_deref().unwrap_or("")
            );
        }
    }

    /// Ask to read the transcript of the run on show, or the newest run of the agent.
    /// The terminal loop carries it out in the user's own `$PAGER`.
    pub fn request_view_transcript(&mut self) {
        let (path, label) = if self.screen == Screen::Runs {
            match self.selected_run() {
                Some(run) => (
                    run.dir.join(crate::transcript::TRANSCRIPT_FILE),
                    format!("the transcript of run {} of `{}`", run.run_id, run.agent),
                ),
                None => {
                    self.status = "there is no run to open".to_owned();
                    return;
                }
            }
        } else {
            let Some(name) = self.selected_name() else {
                self.status = "there is no agent whose transcript could be opened".to_owned();
                return;
            };
            match self.latest_run_dir(&name) {
                Some(dir) => (
                    dir.join(crate::transcript::TRANSCRIPT_FILE),
                    format!("the newest transcript of `{name}`"),
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
            before: None,
        });
        // Name the way out before the pager takes the screen: `esc` cancels the
        // product's own overlays but not the user's pager, whose own key is `q` (M-5).
        self.status = format!("opening {label} — leave it with q");
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
            before: None,
        });
        // Name the way out before the pager takes the screen: `esc` cancels the
        // product's own overlays but not the user's pager, whose own key is `q` (M-5).
        self.status = format!("opening {label} — leave it with q");
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
                // Compare the file before and after: never claim an edit that was not
                // written. A person who quit the editor without saving must be told so.
                let changed = action.before != stamp(&action.path);
                let program = crate::editor::editor();
                self.reload();
                if !status.success() {
                    self.status = format!(
                        "{program} left with code {:?} for {}; it may not have been saved",
                        status.code(),
                        action.label
                    );
                } else {
                    // `declared steps` is plural; `instruction` and `new skill` are not.
                    let verb = if action.label.contains("steps") || action.label.contains("skills")
                    {
                        "were"
                    } else {
                        "was"
                    };
                    if changed {
                        self.status = format!(
                            "{} {verb} edited in {program}; the files were read again",
                            action.label
                        );
                    } else {
                        self.status =
                            format!("{} {verb} not changed ({program})", action.label);
                    }
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

    /// The editor's emergency save for the selected instruction, when one exists.
    ///
    /// nano writes `<name>.save` on an interrupted edit and vim writes `.‹name›.swp`;
    /// the words are not lost, but the product has to name the file for a
    /// zero-knowledge user to find them.
    pub fn instruction_backup(&self) -> Option<PathBuf> {
        let paths = self.selected_paths()?;
        editor_backup(paths.instruction())
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

    /// The instruction's size on disk, the size a run sends, and whether the difference
    /// is the product's guidance line.
    ///
    /// A run drops the product's guidance line; the screen shows both numbers and the
    /// *real* reason for the difference (P-4, T-3, D-1): a document with no guidance
    /// line but a trailing newline differs by that byte, and saying a line was dropped
    /// would send the user looking for a line they never wrote.
    pub fn instruction_payload(&self) -> Option<(u64, u64, bool)> {
        let paths = self.selected_paths()?;
        let bytes = std::fs::read(paths.instruction()).ok()?;
        let (on_disk, sent) = crate::agent_run::instruction_payload(&bytes);
        Some((
            on_disk as u64,
            sent as u64,
            crate::agent_run::guidance_dropped(&bytes),
        ))
    }

    /// Whether the instruction document holds nothing but the guidance placeholder.
    ///
    /// The page map distinguishes the empty instruction from the written one; a file
    /// that is only the template is still empty, and reporting its byte count as
    /// content makes a person believe they wrote something they did not.
    pub fn instruction_is_only_placeholder(&self) -> bool {
        let Some(paths) = self.selected_paths() else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(paths.instruction()) else {
            return false;
        };
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        match lines.next() {
            Some(line) => {
                line == crate::agent_run::INSTRUCTION_PLACEHOLDER && lines.next().is_none()
            }
            None => false,
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
        self.note_stop_proven();
        let stale = match self.last_scan {
            Some(at) => at.elapsed() >= RECENT_RUNS_REFRESH,
            None => true,
        };
        if stale {
            self.refresh_recent();
        }
    }

    /// When a signalled stop's record has closed, say so in the past tense.
    ///
    /// The row already reads `stopped`; leaving the status at `stopping` makes a
    /// person who watches the state cell think the stop is not finished. This is the
    /// one place the two are brought back into agreement, once the record is on disk.
    fn note_stop_proven(&mut self) {
        let Some(name) = self.pending_stop.clone() else {
            return;
        };
        let proven = matches!(
            self.supervisor.state(&name),
            State::Finished {
                outcome: RunOutcome::Stopped,
                ..
            }
        ) || (self.supervisor.mode(&name) == Mode::Systemd
            && !self.supervisor.state(&name).is_live());
        if proven {
            self.status = format!(
                "stopped `{name}`: its process group was signalled and is proven gone"
            );
            self.pending_stop = None;
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
        self.panel_scroll = 0;
    }

    /// Show one screen by name.
    pub fn show(&mut self, screen: Screen) {
        if self.screen != screen {
            self.panel_scroll = 0;
        }
        self.screen = screen;
    }

    /// Whether the screen on show draws a list whose arrows move a selection.
    ///
    /// On the screens that do not — Agent, Instruction, Credentials — the arrows and
    /// page keys move the panel itself, so a key is never swallowed without a word or
    /// an effect (Y-2).
    fn screen_has_list(&self) -> bool {
        matches!(self.screen, Screen::Agents | Screen::Runs | Screen::Forgejo)
    }

    /// Scroll the panel or note area on show by `steps` logical lines.
    ///
    /// The offset names a line, not a terminal row, so a wrapped line moves as one and
    /// the last line can always be brought to the top: the tail of every overflowing
    /// panel is reachable at the advertised minimum (Y-2).
    pub fn scroll_panel(&mut self, steps: isize) {
        let total = crate::ui::panel_total_lines(self);
        if total == 0 {
            self.status =
                "this screen's content is a list: up/down move it, and the panel below is declared — enlarge the window to read it"
                    .to_owned();
            return;
        }
        let last = total.saturating_sub(1);
        let before = self.panel_scroll.min(last);
        let after = (before as isize + steps).clamp(0, last as isize) as usize;
        self.panel_scroll = after;
        if after == before {
            self.status = if steps < 0 {
                "the panel is at its top".to_owned()
            } else {
                "the panel is at its end".to_owned()
            };
        } else {
            let back = if steps < 0 { "PageDown or ]" } else { "PageUp or [" };
            self.status = format!("the panel scrolled — press {back} to go back");
        }
    }

    /// Move the panel on show to its first or last line.
    pub fn scroll_panel_edge(&mut self, last: bool) {
        let total = crate::ui::panel_total_lines(self);
        if total == 0 {
            self.status =
                "this screen's content is a list: up/down move it, and the panel below is declared — enlarge the window to read it"
                    .to_owned();
            return;
        }
        if last {
            self.panel_scroll = total.saturating_sub(1);
            self.status = "the panel is at its end".to_owned();
        } else {
            self.panel_scroll = 0;
            self.status = "the panel is at its top".to_owned();
        }
    }

    /// Move the selection on the screen on show.
    pub fn move_selection(&mut self, steps: isize) {
        match self.screen {
            Screen::Runs => self.move_run_selection(steps),
            Screen::Forgejo => self.move_repo_selection(steps),
            _ => self.move_agent_selection(steps),
        }
    }

    /// Move the selection to the first or last row of the list on show.
    ///
    /// Home/`g` and End/`G` mean the same thing on every list, so a person who reads
    /// the end by reflex is not sent somewhere else (C-4).
    pub fn select_edge(&mut self, last: bool) {
        match self.screen {
            Screen::Runs => {
                self.run_selected = if last {
                    self.recent.len().saturating_sub(1)
                } else {
                    0
                };
            }
            Screen::Forgejo => {
                let count = match &self.role_check {
                    Some(Ok(check)) => check.repositories.len(),
                    _ => 0,
                };
                self.repo_selected = if last { count.saturating_sub(1) } else { 0 };
            }
            _ => {
                self.selected = if last {
                    self.agents().len().saturating_sub(1)
                } else {
                    0
                };
            }
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

    /// Open the selected agent's configuration screen, or the add-agent form when the
    /// list is empty: `enter` must never be a silent dead end either.
    fn open_selected_agent(&mut self) {
        if self.selected_agent().is_some() {
            self.screen = Screen::AgentConfig;
        } else if self.agents().is_empty() {
            self.request_new_agent();
        } else {
            self.status = "the configuration lists no agent to open".to_owned();
        }
    }

    /// One key press.
    /// One bracketed paste, delivered by the terminal as a single event.
    ///
    /// A paste is not a burst of keystrokes: the terminal tells the program where it
    /// starts and ends. Every field this product has takes one line, so the text
    /// before the first line break is kept — never joined with the next line — the
    /// rest is named as unused, and the field is **replaced**, so pasting twice does
    /// not silently double a secret. This is what P-1 and P-2 asked for; without
    /// bracketed paste the program could not tell a paste from typing.
    pub fn on_paste(&mut self, text: &str) {
        if self.show_acknowledgment || self.show_help {
            return;
        }
        let (first, more) = match text.find(['\n', '\r']) {
            Some(at) => (&text[..at], true),
            None => (text, false),
        };
        let note = if more {
            "kept the first line; the rest of the paste was not used"
        } else {
            "pasted into the field"
        };
        if let Some(form) = self.add_agent.as_mut() {
            form.fields[form.cursor] = first.to_owned();
            self.status = note.to_owned();
            return;
        }
        if let Some(prompt) = self.secret_prompt.as_mut() {
            prompt.value = first.to_owned();
            self.status = note.to_owned();
            return;
        }
        // A paste is never dropped in silence; say where it can land.
        self.status =
            "there is no one-line field here to take a paste; press n for the form, or k/f/v/A for a secret"
                .to_owned();
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        // A control chord is never a plain-letter binding. `^C` is the reflex keystroke
        // for "stop"; here it leaves the UI, and the children it owns are stopped on
        // the way out — the same destructive act `q` performs. So it takes the same
        // question `q` does and leaves only on a second press (C-1). Before this,
        // `^C` left the whole tool and killed every run without a word.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('c') {
                self.ask_to_leave("ctrl-c");
            }
            return;
        }
        // A pending "leave and stop the runs?" is cancelled by any key but `q`, and
        // the standing status is replaced: a warning that keeps asking a question
        // already answered reads as a question the product did not hear (D-6).
        if self.confirm_quit.is_some() && key.code != KeyCode::Char('q') {
            self.confirm_quit = None;
            self.status =
                "the run continues and this UI stays; q will ask again before leaving".to_owned();
        }
        if self.show_help {
            match key.code {
                // The help is a scrollable list, so a short window can still reach
                // every key instead of clipping the last group (P-6). The list keys
                // a person brings from anywhere work here too, and one the overlay
                // cannot use says so rather than vanishing into it (C-4).
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_offset = self.help_offset.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_offset = self.help_offset.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_offset = self.help_offset.saturating_add(HELP_PAGE);
                }
                KeyCode::PageUp => {
                    self.help_offset = self.help_offset.saturating_sub(HELP_PAGE);
                }
                KeyCode::Home | KeyCode::Char('g') => self.help_offset = 0,
                KeyCode::End | KeyCode::Char('G') => self.help_offset = usize::MAX,
                KeyCode::Esc
                | KeyCode::Char('?')
                | KeyCode::Char('h')
                | KeyCode::Char('q')
                | KeyCode::Enter => {
                    self.show_help = false;
                }
                _ => {
                    self.status =
                        "the help scrolls with up/down or a page with PageDown/PageUp; esc closes it"
                            .to_owned();
                }
            }
            return;
        }
        if self.add_agent.is_some() {
            self.on_form_key(key);
            return;
        }
        if self.editor_confirm.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.pending = self.editor_confirm.take();
                    if let Some(action) = &self.pending {
                        self.status = format!("opening {}", action.label);
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.editor_confirm = None;
                    self.status = "the file was not opened".to_owned();
                }
                _ => {}
            }
            return;
        }
        if self.secret_prompt.is_some() {
            self.on_secret_key(key);
            return;
        }
        if self.confirm_remove.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm_remove_agent();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cancel_remove_agent();
                }
                _ => {}
            }
            return;
        }
        if self.confirm_forget.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm_forget_secrets();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirm_forget = None;
                    self.status = "the leftover secrets were kept".to_owned();
                }
                _ => {}
            }
            return;
        }
        if self.show_acknowledgment {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_acknowledgment(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.decline_acknowledgment();
                }
                KeyCode::Char('q') => self.should_quit = true,
                // The modal is correctly modal, but a swallowed key must not be
                // silent: say what answers this screen, so a person who pressed `s`
                // first is not left to guess.
                _ => {
                    self.status =
                        "answer this screen first: y to acknowledge, or n for not now".to_owned();
                }
            }
            return;
        }

        match key.code {
            // Leaving ends every `tui` run the UI started; say so and ask once, so a
            // quit is not a silent stop of work in flight (W-10).
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.shift_screen(1),
            KeyCode::BackTab | KeyCode::Left => self.shift_screen(-1),
            // `?` and `h` open the product's own key reference.
            KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => self.toggle_help(),
            // `n` and `a` are the obvious "new agent" keys, and are not silent.
            KeyCode::Char('n') | KeyCode::Char('a') => self.request_new_agent(),
            // `T` opens the transcript, the run's own record beside the log.
            KeyCode::Char('T') => self.request_view_transcript(),
            // `C` runs the role check with the administrator token, so the two roles
            // can be compared in the app.
            KeyCode::Char('C') => self.request_admin_role_check(),
            KeyCode::Up => {
                if self.screen_has_list() {
                    self.move_selection(-1);
                } else {
                    self.scroll_panel(-1);
                }
            }
            // `k` moves up everywhere except the Credentials screen, where it opens the
            // prompt for a model key.
            KeyCode::Char('k') if self.screen != Screen::Credentials => {
                if self.screen_has_list() {
                    self.move_selection(-1);
                } else {
                    self.scroll_panel(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.screen_has_list() {
                    self.move_selection(1);
                } else {
                    self.scroll_panel(1);
                }
            }
            // Page, home and end motion on every windowed list and on the help overlay:
            // the conventions a person brings from any other list (C-4). `g` is the
            // same "top" key everywhere except the Forgejo screen, where it is the
            // clone key the page map names; it never acts on a repository from another
            // screen (C-6). On a screen whose content is a panel, the same keys move
            // the panel instead of a selection that is not there, so no key is
            // swallowed without a word or an effect (Y-2).
            KeyCode::PageDown => {
                if self.screen_has_list() {
                    self.move_selection(LIST_PAGE);
                } else {
                    self.scroll_panel(PANEL_PAGE);
                }
            }
            KeyCode::PageUp => {
                if self.screen_has_list() {
                    self.move_selection(-LIST_PAGE);
                } else {
                    self.scroll_panel(-PANEL_PAGE);
                }
            }
            KeyCode::Home => {
                if self.screen_has_list() {
                    self.select_edge(false);
                } else {
                    self.scroll_panel_edge(false);
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if self.screen_has_list() {
                    self.select_edge(true);
                } else {
                    self.scroll_panel_edge(true);
                }
            }
            // `[` and `]` scroll the panel or note area on every screen, including the
            // ones where the arrows and page keys belong to the list (Y-2).
            KeyCode::Char('[') => self.scroll_panel(-1),
            KeyCode::Char(']') => self.scroll_panel(1),
            KeyCode::Enter => self.open_selected_agent(),
            KeyCode::Esc => self.screen = Screen::Agents,
            KeyCode::Char('s') => self.request_agent_start(),
            KeyCode::Char('x') => self.request_agent_stop(),
            KeyCode::Char('m') => self.request_mode_switch(),
            KeyCode::Char('p') => self.request_endpoint_test(),
            KeyCode::Char('c') => self.request_role_check(),
            KeyCode::Char('i') => self.request_identity(),
            KeyCode::Char('g') if self.screen == Screen::Forgejo => self.request_clone(),
            KeyCode::Char('g') => {
                if self.screen_has_list() {
                    self.select_edge(false);
                } else {
                    self.scroll_panel_edge(false);
                }
            }
            KeyCode::Char('P') if self.screen == Screen::Forgejo => {
                self.request_pull_request();
            }
            KeyCode::Char('U') if self.screen == Screen::Forgejo => {
                self.request_update_pull_request();
            }
            KeyCode::Char('e') => self.request_edit(EditTarget::Instruction),
            KeyCode::Char('t') => self.request_edit(EditTarget::Steps),
            KeyCode::Char('o') => self.request_view_log(),
            // `k` moves up; the skills directory is opened with `K`.
            KeyCode::Char('K') => self.request_edit(EditTarget::Skills),
            // `d` removes an agent, after a confirmation; a mistyped agent is no
            // longer a dead end that ends in a text editor.
            KeyCode::Char('d') | KeyCode::Delete => self.request_remove_agent(),
            // A secret needed after the agent exists is supplied here, masked, without
            // handing the credentials file to an editor.
            KeyCode::Char('k') if self.screen == Screen::Credentials => {
                self.request_secret(SecretTarget::ModelKey);
            }
            // `D` deletes the secrets a removed agent left behind, so "what should I
            // clean" has an answer inside the product (Y-6).
            KeyCode::Char('D') if self.screen == Screen::Credentials => {
                self.request_forget_secrets();
            }
            KeyCode::Char('f') if self.screen == Screen::Credentials => {
                self.request_secret(SecretTarget::ForgejoRoleToken);
            }
            KeyCode::Char('v') if self.screen == Screen::Forgejo => {
                self.request_secret(SecretTarget::ForgejoAgentToken);
            }
            KeyCode::Char('A')
                if matches!(self.screen, Screen::Credentials | Screen::Forgejo) =>
            {
                self.request_secret(SecretTarget::ForgejoAdminToken);
            }
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char(digit) if ('1'..='6').contains(&digit) => {
                let at = digit as usize - '1' as usize;
                self.screen = Screen::ALL[at];
            }
            // A press that cannot act must not be silent: say so, and name the way to
            // the full list. Silence reads as a frozen program.
            _ => {
                self.status = "no key is bound to that — press ? for the list".to_owned();
            }
        }
    }
}

/// How long the tool may take to stop everything before it leaves.
const STOP_ALL_BUDGET_SECS: u64 = 10;

/// Render a configuration after adding one agent, preserving every existing field.
///
/// The configuration has no comments of its own to keep once the tool writes it; the
/// file it produces is valid, checked by the caller, and holds no secret.
fn render_config(existing: &Config, url: &str, name: &str, endpoint: &str, model: &str) -> String {
    let mut with_url = existing.clone();
    with_url.forgejo.url = url.to_owned();
    let added = AgentConfig {
        name: name.to_owned(),
        model: model.to_owned(),
        endpoint: endpoint.to_owned(),
        max_turns: crate::config::DEFAULT_MAX_TURNS,
        instruction: PathBuf::from(format!("agents/{name}/instruction.md")),
        concurrency: 1,
    };
    let mut agents = existing.agents.clone();
    agents.push(added);
    render_full_config(&with_url, &agents)
}

/// Render a whole configuration from its parts, preserving every field.
///
/// Used when the tool rewrites the configuration itself — adding an agent or removing
/// one — so the file it writes is valid, carries no secret, and names no URL the user
/// did not give it.
fn render_full_config(existing: &Config, agents: &[AgentConfig]) -> String {
    let mut text = String::new();
    text.push_str("# gewu-multiverse configuration (written by the tool)\n\n");
    text.push_str("[forgejo]\n");
    text.push_str(&format!("url = {}\n", quote(&existing.forgejo.url)));
    text.push_str(&format!("role = {}\n\n", quote(existing.forgejo.role.as_str())));
    text.push_str("[limits]\n");
    text.push_str(&format!(
        "run_timeout_secs = {}\n",
        existing.limits.run_timeout_secs
    ));
    text.push_str(&format!(
        "max_concurrent_agents = {}\n\n",
        existing.limits.max_concurrent_agents
    ));
    for agent in agents {
        write_agent_block(&mut text, agent);
    }
    text
}

/// One `[[agents]]` block.
fn write_agent_block(text: &mut String, agent: &AgentConfig) {
    text.push_str("[[agents]]\n");
    text.push_str(&format!("name = {}\n", quote(&agent.name)));
    text.push_str(&format!("model = {}\n", quote(&agent.model)));
    text.push_str(&format!("endpoint = {}\n", quote(&agent.endpoint)));
    text.push_str(&format!("max_turns = {}\n", agent.max_turns));
    text.push_str(&format!(
        "instruction = {}\n",
        quote(&agent.instruction.display().to_string())
    ));
    text.push_str(&format!("concurrency = {}\n\n", agent.concurrency));
}

/// A TOML string literal for a value the tool writes.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// Create a file that must exist before it can be edited, so the editor never opens
/// on a path that is not there. A skill gets a scratch `SKILL.md`, because a skill is
/// one directory holding that manifest, never a directory handed to an editor.
fn ensure_editable(target: EditTarget, path: &std::path::Path, agent: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = match target {
        // The guidance is a single marked placeholder, not prose in the document: the
        // run drops any line that is exactly this text before it sends the document,
        // so a person who types below the guidance never sends the guidance itself.
        EditTarget::Instruction => format!(
            "{}\n",
            crate::agent_run::INSTRUCTION_PLACEHOLDER
        ),
        EditTarget::Steps => format!(
            "# Declared shell steps for one run of `{agent}`, one command per line.\n\
             # Blank lines and lines starting with # are skipped.\n\
             # These commands run first, in the agent's workspace, before the model is\n\
             # called. Remove the file to declare no steps.\n"
        ),
        EditTarget::Skills => format!(
            "# A skill for the agent `{agent}`.\n\
             #\n\
             # One skill is one directory under `skills/`, and it holds this file,\n\
             # SKILL.md. Write the skill's name and what it does below: a run reads it\n\
             # and installs it into the run's own skills directory.\n\n\
             # name: {agent}\n"
        ),
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

    /// Write model keys, so a start reaches the supervisor rather than being refused
    /// for a missing key.
    fn give_keys(app: &App, agents: &[&str]) {
        let mut text = String::from("[model_keys]\n");
        for agent in agents {
            text.push_str(&format!("\"{agent}\" = \"gw-test-key\"\n"));
        }
        crate::mode::write_private(app.layout.credentials_file(), text.as_bytes())
            .expect("the test writes keys");
    }

    /// Write one model key.
    fn give_key(app: &App, agent: &str) {
        give_keys(app, &[agent]);
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
        give_key(&app, "one");
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
        give_keys(&app, &["one", "two"]);
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
        app.editor_configured = true;
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
        app.editor_configured = true;
        let paths = app.selected_paths().expect("paths");
        app.on_key(key(KeyCode::Char('t')));
        let action = app.take_action().expect("a steps edit");
        assert_eq!(action.path, paths.steps_file());
        assert!(paths.steps_file().is_file());
        // N-10: the steps template must not doubt the product by saying model calls
        // do not exist yet; they do, and the template describes when the steps run.
        let template = std::fs::read_to_string(paths.steps_file()).expect("the steps read");
        assert!(!template.contains("Until model calls exist"), "{template}");
        assert!(template.contains("before the model is"), "{template}");
        app.on_key(key(KeyCode::Char('K')));
        let action = app.take_action().expect("a skills edit");
        // `K` opens a scratch `SKILL.md`, never a directory.
        assert_eq!(action.path, paths.skills_dir().join("new-skill").join("SKILL.md"));
        assert!(action.path.is_file());
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
        give_key(&app, "one");
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

    /// A successful exit status from `true`, for a hand-off that changed nothing.
    fn success() -> std::process::ExitStatus {
        std::process::Command::new("true")
            .status()
            .expect("`true` runs")
    }

    #[test]
    fn an_unchanged_edit_is_reported_as_not_changed() {
        // F-E: never claim an edit that was not written.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        app.on_key(key(KeyCode::Char('e')));
        let action = app.take_action().expect("an edit was asked for");
        app.note_action(&action, Ok(success()));
        assert!(app.status.contains("was not changed"), "{}", app.status);
    }

    #[test]
    fn an_edit_that_changed_the_file_is_reported_as_edited() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        app.on_key(key(KeyCode::Char('e')));
        let action = app.take_action().expect("an edit was asked for");
        std::fs::write(&action.path, "a changed instruction\n").expect("the file is changed");
        app.note_action(&action, Ok(success()));
        assert!(app.status.contains("was edited"), "{}", app.status);
    }

    #[test]
    fn an_unset_editor_asks_before_it_takes_the_screen() {
        // F-D: with no `$EDITOR`, name the program and wait, rather than dropping the
        // user into the editor's own screen with no hint.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = false;
        app.on_key(key(KeyCode::Char('e')));
        assert!(app.editor_confirm.is_some());
        assert!(app.take_action().is_none(), "the editor waits for enter");
        // Esc cancels, leaving the edit unopened.
        app.on_key(key(KeyCode::Esc));
        assert!(app.editor_confirm.is_none());
        // A second attempt, confirmed with enter, becomes a real pending action.
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.take_action().is_some(), "enter opens the editor");
    }

    #[test]
    fn a_missing_model_key_refuses_a_start_before_a_run_is_wasted() {
        // N-8: `p` knows the key is missing; `s` must know too.
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("model key"), "{}", app.status);
        assert!(app.status.contains("model_keys"), "{}", app.status);
        assert_eq!(app.supervisor.running_count(), 0);
    }

    #[test]
    fn a_failed_runs_reason_is_readable_from_the_agent() {
        // F-G: the reason lives on the screen, not only in a transient status.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let run_id = "run-100-0001-1";
        let dir = app.layout.state_root().join("one").join(run_id);
        std::fs::create_dir_all(&dir).expect("the run directory is created");
        let mut transcript =
            crate::transcript::Transcript::open(&dir).expect("a transcript is opened");
        transcript
            .append(&crate::transcript::Record::run_started(run_id, "one", 1, "/w"))
            .expect("a record is appended");
        transcript
            .append(&crate::transcript::Record::run_finished(
                run_id,
                RunOutcome::Failed,
                Some(1),
                5,
                Some("the model call failed (transport): connection refused".to_owned()),
            ))
            .expect("a record is appended");
        app.refresh_recent();
        assert_eq!(
            app.last_failure("one").as_deref(),
            Some("the model call failed (transport): connection refused")
        );
    }

    const NO_AGENTS: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"
"#;

    #[test]
    fn a_reload_forgets_an_outcome_whose_record_is_gone() {
        // T-5: deleting the run records and pressing `r` must put the hub back in step
        // with the ledger, not leave it claiming an outcome no record holds.
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.tick();
            if matches!(app.supervisor.state("one"), State::Finished { .. }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matches!(app.supervisor.state("one"), State::Finished { .. }),
            "the run finished"
        );
        let runs = app.layout.state_root().join("one");
        if runs.exists() {
            std::fs::remove_dir_all(&runs).expect("the records are removed");
        }
        app.reload();
        assert_eq!(
            app.supervisor.state("one"),
            State::Idle,
            "the hub is in step with the ledger"
        );
        assert!(
            app.recent.iter().all(|run| run.agent != "one"),
            "the ledger holds no record"
        );
        // Y-4: the hub knows a record it had is gone, so it can say so instead of
        // silently promoting an older record to "the last run".
        assert_eq!(app.forgotten_run.as_deref(), Some("one"));
        app.shutdown();
    }

    #[test]
    fn an_endpoint_test_is_dated_and_knows_when_the_endpoint_changed() {
        // Y-5: a result with no time outlives the endpoint it tested and reads as
        // current. It is dated, and a result about another endpoint is not current.
        use crate::config::AgentConfig;
        let test = EndpointTest {
            agent: "one".to_owned(),
            endpoint: "http://127.0.0.1:1/v1".to_owned(),
            model: "m".to_owned(),
            result: Ok("the endpoint answered".to_owned()),
            tested_at_ms: crate::time::now_unix_ms(),
        };
        assert!(test.describe().contains("tested just now"), "{}", test.describe());
        let same = AgentConfig {
            name: "one".to_owned(),
            model: "m".to_owned(),
            endpoint: "http://127.0.0.1:1/v1".to_owned(),
            max_turns: 4,
            instruction: std::path::PathBuf::from("instruction.md"),
            concurrency: 1,
        };
        assert!(test.is_current_for(&same));
        let changed = AgentConfig {
            endpoint: "http://127.0.0.1:2/v1".to_owned(),
            ..same
        };
        assert!(!test.is_current_for(&changed));
    }

    #[test]
    fn a_failed_start_is_not_called_a_failed_run() {
        // D-8: the unit could not be started, so there was no run to speak of.
        let mut app = app_with(GOOD);
        app.start_failure = Some(("one".to_owned(), "systemd could not start it".to_owned()));
        assert!(app.last_run_failure("one").is_none());
        assert_eq!(
            app.last_start_failure("one").as_deref(),
            Some("systemd could not start it")
        );
    }

    #[test]
    fn the_last_run_is_the_newest_record_not_the_newest_failure() {
        // O-1: after the network returned, a fresh run succeeded and the hub still
        // said "the last run ... did not go well", because it searched for the newest
        // failure rather than reading the newest record. The Agent screen's
        // `last failure` keeps the history; the hub's sentence is about the last run.
        let mut app = app_with(GOOD);
        let failed = RunSummary {
            agent: "one".to_owned(),
            run_id: "run-100-0001-1".to_owned(),
            outcome: Some(RunOutcome::Failed),
            reason: Some(
                "the model call failed (transport): io: Connection refused (os error 111)"
                    .to_owned(),
            ),
            duration_ms: Some(0),
            started_at: None,
            log: PathBuf::from("/l"),
            dir: PathBuf::from("/d"),
        };
        let succeeded = RunSummary {
            agent: "one".to_owned(),
            run_id: "run-200-0001-1".to_owned(),
            outcome: Some(RunOutcome::Succeeded),
            reason: Some("the model finished after 1 turn".to_owned()),
            duration_ms: Some(10),
            started_at: None,
            log: PathBuf::from("/l"),
            dir: PathBuf::from("/d"),
        };
        app.recent = vec![succeeded.clone(), failed.clone()];
        assert!(
            app.last_run_failure("one").is_none(),
            "an older failure is not the last run"
        );
        assert_eq!(
            app.last_failure("one").as_deref(),
            failed.reason.as_deref(),
            "the historical failure is still named on the Agent screen"
        );
        // When the newest record is the failure, it is named.
        app.recent = vec![failed.clone(), succeeded];
        assert_eq!(
            app.last_run_failure("one").as_deref(),
            failed.reason.as_deref()
        );
    }

    #[test]
    fn page_and_edge_keys_move_the_selection_on_every_list() {
        // C-4: PageDown/PageUp/Home/End/G are the list conventions a person brings
        // from anywhere, and an unbound key inside the help overlay speaks instead of
        // vanishing into it.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::End));
        assert_eq!(app.selected, app.agents().len() - 1);
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.selected, app.agents().len() - 1);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.selected, 0);

        app.on_key(key(KeyCode::Char('?')));
        app.on_key(key(KeyCode::PageDown));
        assert!(app.help_offset > 0, "PageDown did not page the help");
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.help_offset, 0);
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.status.contains("help scrolls"), "{}", app.status);
    }

    #[test]
    fn the_clone_key_does_not_fire_from_another_screen() {
        // C-6: `g` on the Agents list did a Forgejo clone and told the user to run the
        // role check while they were looking at agents. Off the Forgejo screen it is
        // the list's own "top" key; the clone stays on the Forgejo screen.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.select_edge(true);
        assert_ne!(app.selected, 0, "the selection starts at the last row");
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(app.screen, Screen::Agents, "g moved to the Forgejo screen");
        assert_eq!(app.selected, 0, "g is the top-of-list key here");
    }

    #[test]
    fn the_pager_hand_off_names_the_way_out_before_it_opens() {
        // M-5: `esc` is the product's own way back, but the pager's is `q`; say so
        // before the pager takes the screen.
        let root = scratch_root("pager-leave");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration is written");
        let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner("pager"));
        app.show_acknowledgment = false;
        let run_dir = root.join("state").join("one").join("run-100-0001-1");
        std::fs::create_dir_all(&run_dir).expect("the run directory is created");
        std::fs::write(run_dir.join(crate::run::LOG_FILE), "log\n").expect("the log");
        app.recent = vec![RunSummary {
            agent: "one".to_owned(),
            run_id: "run-100-0001-1".to_owned(),
            outcome: Some(RunOutcome::Succeeded),
            reason: None,
            duration_ms: Some(1),
            started_at: None,
            log: run_dir.join(crate::run::LOG_FILE),
            dir: run_dir,
        }];
        app.run_selected = 0;
        app.show(Screen::Runs);
        app.request_view_log();
        assert!(
            app.status.contains("leave it with q"),
            "{}",
            app.status
        );
    }

    #[test]
    fn an_abandoned_run_is_closed_as_stopped_not_shown_as_idle() {
        // S-1: a `tui` run is a child of the UI. A reopened UI must not call the agent
        // idle while the run's own record says it never ended.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let run_id = "run-100-0001-2";
        let dir = app.layout.state_root().join("one").join(run_id);
        std::fs::create_dir_all(&dir).expect("the run directory is created");
        let mut transcript =
            crate::transcript::Transcript::open(&dir).expect("a transcript is opened");
        // A process id that cannot be running.
        transcript
            .append(&crate::transcript::Record::run_started(
                run_id,
                "one",
                4_000_000_000,
                "/w",
            ))
            .expect("a record is appended");
        app.reload();
        let state = app.supervisor.state("one");
        assert!(!state.is_live());
        assert_eq!(state.word(), "stopped");
        assert!(crate::transcript::terminal_record(&dir).is_some());
    }

    #[test]
    fn removing_an_agent_asks_first_and_writes_the_configuration() {
        // C-4: a mistyped agent must be removable inside the product.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.confirm_remove.as_deref(), Some("one"));
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.agents().iter().any(|agent| agent.name == "one"));
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('y')));
        assert!(!app.agents().iter().any(|agent| agent.name == "one"));
        let text = std::fs::read_to_string(app.config_path()).expect("config readable");
        assert!(text.contains("name = \"two\""), "{text}");
        assert!(!text.contains("name = \"one\""), "{text}");
    }

    #[test]
    fn a_running_agent_cannot_be_removed_and_its_run_stays_visible() {
        // W-2 / W-9: `d` on a running agent used to remove the row and orphan the run
        // — counted by the footer, invisible on the hub, unreachable by `x`. The
        // removal is refused now, the agent stays, and the count does not move.
        let mut app = app_with_sleeping_runner("app-remove-live", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");
        let running_before = app.supervisor.running_count();
        assert_eq!(running_before, 1);

        app.on_key(key(KeyCode::Char('d')));
        assert!(
            app.confirm_remove.is_none(),
            "the removal must be refused, not confirmed"
        );
        assert!(app.status.contains("refused"), "{}", app.status);
        assert!(app.status.contains("stop it first"), "{}", app.status);
        assert!(app.status.contains("run"), "{}", app.status);
        assert!(
            app.agents().iter().any(|agent| agent.name == "one"),
            "the agent is still configured"
        );
        assert_eq!(
            app.supervisor.running_count(),
            running_before,
            "the count still matches the one run in flight"
        );
        app.shutdown();
    }

    #[test]
    fn an_agent_removed_from_the_file_does_not_leave_an_unseen_run() {
        // The orphaned-run class through the reload door: hand-edit the file, press
        // `r`, and the run must not stay counted but invisible.
        let only_two = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "two"
model = "m"
endpoint = "https://m.example.org"
instruction = "two.md"
concurrency = 3
"#;
        let mut app = app_with_sleeping_runner("app-reload-orphan", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");

        std::fs::write(app.config_path(), only_two).expect("the file is hand-edited");
        app.reload();
        assert!(
            !app.agents().iter().any(|agent| agent.name == "one"),
            "the agent is gone from the configuration"
        );
        // The stop is signalled on reload; the poll proves it gone, as it does for a
        // stop asked for with `x`.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && (app.supervisor.state("one").is_live() || app.supervisor.running_count() > 0)
        {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !app.supervisor.state("one").is_live(),
            "its run must not stay live and unseen"
        );
        assert_eq!(app.supervisor.running_count(), 0, "nothing runs unseen");
        assert!(
            app.config_note
                .as_deref()
                .unwrap_or("")
                .contains("no longer lists"),
            "{:?}",
            app.config_note
        );
        // T-10: the note lives in the note area only; the status line must not draw
        // the same sentence a second time.
        assert!(
            !app.status.contains("no longer lists"),
            "the reload note is drawn twice: {}",
            app.status
        );
        // T-6: the run's own record must name the configuration re-read, never
        // "on request" — the user did not press `x`.
        let runs = app.layout.state_root().join("one");
        let log = std::fs::read_dir(&runs)
            .expect("the run directory is there")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(crate::run::LOG_FILE))
            .find_map(|path| std::fs::read_to_string(path).ok())
            .expect("a run log");
        assert!(log.contains("no longer lists"), "{log}");
        assert!(!log.contains("on request"), "{log}");
        app.shutdown();
    }

    #[test]
    fn starting_a_running_agent_says_that_it_is_running() {
        // W-11: the refusal must lead with the state, not the run id.
        let mut app = app_with_sleeping_runner("app-start-live", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("already running"), "{}", app.status);
        assert!(app.status.contains("press x"), "{}", app.status);
        app.shutdown();
    }

    #[test]
    fn quitting_with_a_run_in_flight_asks_once_and_names_what_stops() {
        // W-10: `q` used to end a run without a word.
        let mut app = app_with_sleeping_runner("app-quit-live", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");

        app.on_key(key(KeyCode::Char('q')));
        assert!(!app.should_quit, "the first q asks, it does not leave");
        assert!(app.status.contains("stops"), "{}", app.status);
        assert!(app.status.contains("press q again"), "{}", app.status);

        // Any other key cancels the pending quit. Tab switches screens without setting
        // a status of its own, which is exactly the case D-6 was about.
        app.on_key(key(KeyCode::Tab));
        assert!(!app.should_quit);
        assert!(app.confirm_quit.is_none());
        // D-6: the status no longer re-asks the question the user answered; T-9: the
        // product's own voice does not carry a pluralisation gutter.
        assert!(
            !app.status.contains("press q again"),
            "the answered warning stays: {}",
            app.status
        );
        assert!(app.status.contains("continues"), "{}", app.status);
        assert!(!app.status.contains("run(s)"), "{}", app.status);

        app.on_key(key(KeyCode::Char('q')));
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit, "the second q leaves");
        app.shutdown();
    }

    #[test]
    fn ctrl_c_asks_before_it_leaves_with_a_run_in_flight() {
        // C-1: `^C` left the whole tool and killed every run without a word, while `q`
        // asked; the reflex "cancel" chord is the same destructive act and now takes
        // the same question. C-8: one run is `it`, not `them`.
        let mut app = app_with_sleeping_runner("app-ctrl-c-live", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.on_key(ctrl_c);
        assert!(!app.should_quit, "the first ^C asks, it does not leave");
        assert!(
            app.status.contains("press ctrl-c again"),
            "{}",
            app.status
        );
        assert!(app.status.contains("stop it"), "{}", app.status);

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "the second ^C leaves");
        app.shutdown();
    }

    #[test]
    fn switching_to_systemd_does_not_leave_a_pluralisation_gutter() {
        // C-3: the confirmation read `... started the next time it is started (s)`,
        // an `(s)` that is neither a key nor a plural. A stand-in `systemctl` answers
        // the version query so the success branch is the one driven.
        let program = scratch_root("app-mode-string").join("ok-systemctl.sh");
        std::fs::write(
            &program,
            "#!/bin/sh\ncase \"$*\" in *'show -p Version --value'*) echo 255.4 ;; esac\nexit 0\n",
        )
        .expect("the stand-in is written");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is runnable");
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.supervisor
            .set_systemctl(crate::systemd::Systemctl::with_binary(&program));
        app.on_key(key(KeyCode::Char('m')));
        assert!(
            app.status.contains("now supervised"),
            "the switch did not succeed: {}",
            app.status
        );
        assert!(
            !app.status.contains("(s)"),
            "the confirmation still carries a gutter: {}",
            app.status
        );
    }

    #[test]
    fn a_failed_clone_names_the_account_before_gits_detail() {
        // W-3: the explanation must be the part that survives the status area.
        let status = clone_failed_status(
            "uxadmin/admin-private",
            "gewu-ghost",
            " (managed)",
            "not_found",
            "Cloning into 'admin-private'...\nremote: Repository not found",
        );
        let account = status.find("gewu-ghost").expect("the account is named");
        let detail = status.find("Cloning into").expect("git's detail is there");
        assert!(account < detail, "{status}");
        assert!(
            status.contains("the list came from the role token"),
            "{status}"
        );
    }

    #[test]
    fn an_abandoned_run_reports_the_time_it_actually_ran() {
        // W-6: a run left behind by a dead UI is closed with its real duration, not
        // `0.0s`.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let run_id = "run-100-0001-9";
        let dir = app.layout.state_root().join("one").join(run_id);
        std::fs::create_dir_all(&dir).expect("the run directory is created");
        let started = crate::time::now_unix_ms().saturating_sub(21_000);
        let line = format!(
            "{{\"event\":\"run_started\",\"ts\":\"{}\",\"run_id\":\"{run_id}\",\"agent\":\"one\",\"pid\":4000000000,\"workspace\":\"/w\"}}\n",
            crate::time::rfc3339(started)
        );
        std::fs::write(dir.join(crate::transcript::TRANSCRIPT_FILE), line)
            .expect("the opening record is written");
        app.reload();
        match app.supervisor.state("one") {
            State::Finished { duration_ms, .. } => {
                assert!(duration_ms >= 20_000, "duration was {duration_ms} ms");
            }
            other => panic!("expected a finished state, got {other:?}"),
        }
    }

    #[test]
    fn a_paste_into_a_one_line_field_keeps_one_line_and_replaces() {
        // P-1: two lines pasted into a one-line field are never joined. P-2: a second
        // paste replaces the field instead of doubling it.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('n')));
        app.on_paste("scribe\nendpoint = http://evil.example");
        let form = app.add_agent.as_ref().expect("the form is open");
        assert_eq!(form.fields[0], "scribe");
        assert!(!form.fields[0].contains('\n'));
        assert!(app.status.contains("first line"), "{}", app.status);
        app.on_paste("scribe");
        assert_eq!(app.add_agent.as_ref().expect("the form").fields[0], "scribe");
        app.on_key(key(KeyCode::Esc));

        app.show(Screen::Credentials);
        app.on_key(key(KeyCode::Char('k')));
        app.on_paste("secret-one\nsecret-two");
        app.on_paste("secret-two");
        assert_eq!(
            app.secret_prompt.as_ref().expect("the prompt is open").value,
            "secret-two"
        );
    }

    #[test]
    fn the_form_requires_a_model_key() {
        // S-2: the form must not create an agent the product cannot run.
        let root = scratch_root("app-form-key");
        std::fs::write(root.join("config.toml"), NO_AGENTS).expect("written");
        let mut app =
            App::with_runner(Layout::rooted_at(root), quiet_runner("app-form-key-runner"));
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('n')));
        for character in "scribe".chars() {
            app.on_key(key(KeyCode::Char(character)));
        }
        app.on_key(key(KeyCode::Tab));
        for character in "http://127.0.0.1:9/v1".chars() {
            app.on_key(key(KeyCode::Char(character)));
        }
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Char('m')));
        // The model key is left empty on purpose.
        app.on_key(key(KeyCode::Enter));
        assert!(app.adding_agent());
        assert!(app.status.contains("model key"), "{}", app.status);
        assert!(app.agents().is_empty());
    }

    #[test]
    fn a_very_long_invalid_name_puts_the_reason_first_and_does_not_echo_it() {
        // S-5: a long invalid name used to push the reason off the status area.
        let root = scratch_root("app-long-name");
        std::fs::write(root.join("config.toml"), NO_AGENTS).expect("written");
        let mut app =
            App::with_runner(Layout::rooted_at(root), quiet_runner("app-long-name-runner"));
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('n')));
        for _ in 0..200 {
            app.on_key(key(KeyCode::Char('a')));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.starts_with("cannot use that name"), "{}", app.status);
        assert!(app.status.contains("longer than 32"), "{}", app.status);
        assert!(!app.status.contains("aaaa"), "{}", app.status);
    }

    #[test]
    fn a_secret_prompt_writes_the_key_and_never_shows_it() {
        // C-5: a secret needed after the agent exists is supplied in the product.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.show(Screen::Credentials);
        app.on_key(key(KeyCode::Char('k')));
        assert!(app.secret_prompt.is_some());
        for character in "sk-secret-xyz".chars() {
            app.on_key(key(KeyCode::Char(character)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.secret_prompt.is_none());
        let text = std::fs::read_to_string(&app.credentials_path).expect("credentials");
        assert!(text.contains("sk-secret-xyz"), "{text}");
        assert!(!app.status.contains("sk-secret-xyz"), "{}", app.status);
    }

    #[test]
    fn control_c_leaves_the_ui_and_does_not_run_the_role_check() {
        // S-3: a control chord must never fall through to a plain-letter binding.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
        assert!(app.role_check.is_none());
    }

    #[test]
    fn a_plural_file_is_reported_with_a_plural_verb() {
        // C-11: `the declared steps … were not changed`.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.steps_file(), "echo one\n").expect("the steps file");
        let action = PendingAction {
            kind: ActionKind::Edit,
            path: paths.steps_file().to_path_buf(),
            label: "the declared steps of `one`".to_owned(),
            before: stamp(paths.steps_file()),
        };
        app.note_action(&action, Ok(success()));
        assert!(app.status.contains("were not changed"), "{}", app.status);
    }

    #[test]
    fn a_failed_administrator_check_keeps_the_last_good_role_view() {
        // S-10: `C` without an administrator token must not wipe the member view.
        use crate::config::Role;
        use crate::forgejo::{Permissions, Repository, RoleCheck, Viewer};
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let repository = Repository {
            full_name: "uxmember/demo-public".to_owned(),
            namespace: "uxmember".to_owned(),
            name: "demo-public".to_owned(),
            private: false,
            default_branch: None,
            html_url: None,
            permissions: Permissions::default(),
        };
        app.role_check = Some(Ok(RoleCheck {
            url: "https://forgejo.example.org".to_owned(),
            role: Role::Member,
            viewer: Viewer {
                login: "uxmember".to_owned(),
                email: None,
                description: None,
                is_admin: false,
            },
            repositories: vec![repository],
            private_visible: 0,
        }));
        app.request_admin_role_check();
        assert!(app.status.contains("administrator token"), "{}", app.status);
        assert!(
            matches!(app.role_check, Some(Ok(_))),
            "the good member view was wiped: {:?}",
            app.role_check
        );
        assert!(
            app.forgejo_note
                .as_deref()
                .is_some_and(|note| note.contains("administrator token")),
            "the failure is not shown beside the view"
        );
    }

    /// Write one finished run's records for an agent.
    fn write_finished_run(
        app: &App,
        agent: &str,
        run_id: &str,
        outcome: RunOutcome,
        duration_ms: u64,
        reason: Option<&str>,
    ) {
        let dir = app.layout.state_root().join(agent).join(run_id);
        std::fs::create_dir_all(&dir).expect("the run directory is created");
        let mut transcript =
            crate::transcript::Transcript::open(&dir).expect("a transcript is opened");
        transcript
            .append(&crate::transcript::Record::run_started(run_id, agent, 1, "/w"))
            .expect("a record is appended");
        transcript
            .append(&crate::transcript::Record::run_finished(
                run_id,
                outcome,
                Some(0),
                duration_ms,
                reason.map(str::to_owned),
            ))
            .expect("a record is appended");
    }

    #[test]
    fn a_reopened_ui_shows_the_newest_terminal_outcome_not_idle() {
        // I-1: the hub is where a person lands after a restart; it must not forget.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        write_finished_run(
            &app,
            "one",
            "run-100-0001-9",
            RunOutcome::Failed,
            500,
            Some("the model call failed (transport): connection refused"),
        );
        app.reload();
        assert_eq!(app.supervisor.state("one").word(), "failed");
        assert!(
            app.supervisor
                .state("one")
                .word_with_reason()
                .contains("transport"),
            "{}",
            app.supervisor.state("one").word_with_reason()
        );
        // An agent with no record is still idle: the seed does not invent state.
        assert_eq!(app.supervisor.state("two").word(), "idle");
    }

    #[test]
    fn an_instruction_holding_only_the_placeholder_is_reported_as_empty() {
        // I-4: a byte count is not content; the template is the empty state.
        let app = app_with(GOOD);
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(
            paths.instruction(),
            format!("{}\n", crate::agent_run::INSTRUCTION_PLACEHOLDER),
        )
        .expect("the template is written");
        assert!(app.instruction_is_only_placeholder());
        std::fs::write(paths.instruction(), "write a note\n").expect("an instruction is written");
        assert!(!app.instruction_is_only_placeholder());
    }

    #[test]
    fn an_interrupted_editors_emergency_save_is_named() {
        // I-3: the words were not lost, but a zero-knowledge user cannot find them.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "the placeholder was overwritten\n")
            .expect("the instruction is written");
        let backup = paths.instruction().with_extension("md.save");
        std::fs::write(&backup, "half a paragraph\n").expect("the emergency save is written");
        assert_eq!(app.instruction_backup(), Some(backup.clone()));
        app.on_key(key(KeyCode::Char('e')));
        assert!(app.status.contains("not saved"), "{}", app.status);
        assert!(
            app.status.contains("instruction.md.save"),
            "{}",
            app.status
        );
    }

    #[test]
    fn a_credential_for_a_removed_agent_is_marked_as_belonging_to_none() {
        // X-6: a name with no configured agent is a ghost a person audits.
        let app = app_with(GOOD);
        let text = "[model_keys]\n\"gone\" = \"gw-x\"\n\"one\" = \"gw-y\"\n";
        crate::mode::write_private(app.layout.credentials_file(), text.as_bytes())
            .expect("the credentials are written");
        let labels = app.present_secret_labels();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("`gone`") && label.contains("no agent")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == "model key for `one`"),
            "{labels:?}"
        );
    }

    #[test]
    fn the_footer_counts_what_finished_since_this_ui_started() {
        // I-5: the answer to "what happened while I was looking elsewhere".
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.started_ms = 0;
        write_finished_run(&app, "one", "run-100-0002-1", RunOutcome::Succeeded, 5, None);
        app.refresh_recent();
        assert_eq!(app.runs_finished_since_start(), 1);
        // A run that ended before this UI started is not "since".
        app.started_ms = crate::time::now_unix_ms().saturating_add(1_000_000);
        assert_eq!(app.runs_finished_since_start(), 0);
    }

    #[test]
    fn the_stop_status_agrees_with_the_row_once_the_record_is_closed() {
        // X-7: the row reads `stopped` while the status said `stopping`.
        let mut app = app_with_sleeping_runner("app-stop-status", GOOD);
        app.on_key(key(KeyCode::Char('y')));
        let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
        std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !app.supervisor.state("one").is_live() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.supervisor.state("one").is_live(), "the agent is running");
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.status.contains("stopping"), "{}", app.status);
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && app.supervisor.state("one").word() != "stopped" {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        app.tick();
        assert_eq!(app.supervisor.state("one").word(), "stopped");
        assert!(app.status.starts_with("stopped"), "{}", app.status);
        assert!(app.status.contains("is proven gone"), "{}", app.status);
        app.shutdown();
    }
}
