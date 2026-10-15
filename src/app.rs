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

use std::path::{Path, PathBuf};
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
use crate::words;
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

    /// Whether the label is a plural noun, so a sentence built around it takes a
    /// plural verb (`the declared steps … open`, `the instruction … opens`) (F6).
    pub fn is_plural(self) -> bool {
        matches!(self, EditTarget::Steps)
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

/// A one-field, **visible** prompt for the configured Forgejo address.
///
/// The address is not a secret: the product drew it on the Forgejo screen long before
/// this prompt existed, and a person cannot correct a typo in a masked field. It is
/// its own prompt because it was previously settable only as a side-effect of adding
/// an agent, which silently re-pointed every existing agent at once (W32-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlPrompt {
    /// What has been typed so far, drawn as itself.
    pub value: String,
    /// Why the last attempt to write it did not work.
    pub error: Option<String>,
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
    /// Which of an agent's files an edit opens. `None` for a view.
    pub target: Option<EditTarget>,
    /// Whether the hand-off itself created the file it opened (H-1, H-2).
    pub created: bool,
    /// The template written when the path does not exist yet, so nothing is created
    /// until the hand-off actually begins. `None` for a view.
    pub template: Option<Vec<u8>>,
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
    /// The machine that made the test, when the record names one.
    ///
    /// A test result belongs to a machine and a time (W32-2): the configuration
    /// directory can be copied to a second machine, and a record with no machine in
    /// it is read there as a test this machine never made. `None` means the record
    /// does not name a machine — an older record — and the surface says so rather
    /// than claiming either way.
    pub machine: Option<String>,
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

    /// How this test's machine relates to the machine reading it (W32-2).
    pub fn machine_relation(&self) -> crate::machine::Relation {
        match &self.machine {
            Some(mark) => crate::machine::relation(mark),
            None => crate::machine::Relation::Unnamed,
        }
    }
}

/// One endpoint test as it is written down, so its stamp survives a restart and ages
/// (LIB-2).
///
/// The record holds no secret: the agent's name, the endpoint and model that were
/// tested, the result (a one-line summary or a failure class and message) and the
/// time. It is written beside the acknowledgment record, under the same custody.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct EndpointTestRecord {
    agent: String,
    endpoint: String,
    model: String,
    tested_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answered: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed_message: Option<String>,
    /// The machine the test was made on, when this version wrote the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine: Option<String>,
}

impl EndpointTestRecord {
    fn of(test: &EndpointTest) -> Self {
        let (answered, failed_class, failed_message) = match &test.result {
            Ok(summary) => (Some(summary.clone()), None, None),
            Err(failure) => (None, Some(failure.class.clone()), Some(failure.message.clone())),
        };
        Self {
            agent: test.agent.clone(),
            endpoint: test.endpoint.clone(),
            model: test.model.clone(),
            tested_at_ms: test.tested_at_ms,
            answered,
            failed_class,
            failed_message,
            machine: test.machine.clone(),
        }
    }

    fn into_test(self) -> EndpointTest {
        let result = match (self.answered, self.failed_class, self.failed_message) {
            (Some(summary), _, _) => Ok(summary),
            (None, Some(class), message) => Err(EndpointFailure {
                class,
                message: message.unwrap_or_default(),
            }),
            // A record that carries neither a result nor a failure is not a usable
            // test: it is surfaced as `unknown` and named on the screen, never drawn
            // as if it were a real success or silently treated as no test at all.
            (None, None, _) => Err(EndpointFailure {
                class: "unknown".to_owned(),
                message: "the stored result is not readable".to_owned(),
            }),
        };
        EndpointTest {
            agent: self.agent,
            endpoint: self.endpoint,
            model: self.model,
            result,
            tested_at_ms: self.tested_at_ms,
            machine: self.machine,
        }
    }
}

/// The shape of the endpoint-test record on disk.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct EndpointTestFile {
    #[serde(default, rename = "tests")]
    tests: Vec<EndpointTestRecord>,
}

/// Read the endpoint tests written beside the acknowledgment record.
///
/// A file that is missing, unreadable or unparsable is simply `no test is known`,
/// never an error on a screen: the Agent screen says `not tested yet` for an agent
/// with no record (LIB-2).
pub fn read_endpoint_tests(path: &Path) -> Vec<EndpointTest> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(file) = toml::from_str::<EndpointTestFile>(&text) else {
        return Vec::new();
    };
    file.tests.into_iter().map(EndpointTestRecord::into_test).collect()
}

/// Write one test into the record, replacing that agent's previous one.
///
/// The file is `0600` like every other file the product writes. A read problem is not
/// fatal: the new test replaces what could not be read, so the record always holds the
/// latest test for each agent.
pub fn write_endpoint_test(path: &Path, test: &EndpointTest) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tests: Vec<EndpointTestRecord> = read_endpoint_tests(path)
        .iter()
        .map(EndpointTestRecord::of)
        .collect();
    tests.retain(|record| record.agent != test.agent);
    tests.push(EndpointTestRecord::of(test));
    tests.sort_by(|left, right| left.agent.cmp(&right.agent));
    let text = toml::to_string(&EndpointTestFile { tests })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::mode::write_private(path, text.as_bytes())
}

/// When and where the role check last ran, as a fact the next start can read.
///
/// W37-1: the v0.1.18 release note promises the role check's result carries a
/// **persisted** stamp (`role_checked_at_ms` + `role_check_machine`). The fields
/// lived only in `App`, so after a real restart the Forgejo screen claimed no check
/// had ever run. This is the record that makes the note true, beside the endpoint
/// test's own record and under the same custody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCheckStamp {
    /// When the check ran, in unix milliseconds.
    pub checked_at_ms: u64,
    /// The machine that ran it, when the record names one.
    pub machine: Option<String>,
}

/// One role-check stamp as it is written down. It holds no secret.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RoleCheckRecord {
    checked_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine: Option<String>,
}

/// The shape of the role-check record on disk.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RoleCheckFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<RoleCheckRecord>,
}

/// Read the role-check stamp written beside the endpoint-test record.
///
/// A file that is missing, unreadable or unparsable is simply `no stamp is known`,
/// never an error on a screen: the Forgejo screen falls back to its own sentence.
pub fn read_role_check_stamp(path: &Path) -> Option<RoleCheckStamp> {
    let text = std::fs::read_to_string(path).ok()?;
    let file = toml::from_str::<RoleCheckFile>(&text).ok()?;
    file.check.map(|record| RoleCheckStamp {
        checked_at_ms: record.checked_at_ms,
        machine: record.machine,
    })
}

/// Write the role check's stamp down, replacing the previous one.
///
/// The file is `0600` like every other file the product writes.
pub fn write_role_check_stamp(path: &Path, stamp: &RoleCheckStamp) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RoleCheckFile {
        check: Some(RoleCheckRecord {
            checked_at_ms: stamp.checked_at_ms,
            machine: stamp.machine.clone(),
        }),
    };
    let text = toml::to_string(&file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::mode::write_private(path, text.as_bytes())
}

/// A byte count in the product's own words, so the file that grows with every run has
/// a size a person can watch (L-3).
///
/// The unit is the one the arithmetic uses: the divisor is 1024, so the label is the
/// binary one (`KiB`, `MiB`, `GiB`), never `KB`. A label that names a different
/// definition from the number under it is the one-definition class again (F-2, its
/// third appearance); `du` reads a byte count in the same binary units, so a person
/// can reconcile the screen with their own tools.
pub fn filesize_text(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    let mib = kib / 1024.0;
    if mib < 1024.0 {
        return format!("{mib:.1} MiB");
    }
    format!("{:.1} GiB", mib / 1024.0)
}

/// How long ago a test ran, in words, so a success cannot masquerade as current.
///
/// Every age string in the product comes from here, so the ladder is one ladder
/// (W-6): an instant is `just now`, then seconds, then minutes, hours and days. The
/// seconds rung is what keeps the first step after `just now` from being a full
/// minute, which read as a jump when a report was watched.
fn tested_age(tested_at_ms: u64) -> String {
    let now = crate::time::now_unix_ms();
    let seconds = now.saturating_sub(tested_at_ms) / 1000;
    match seconds {
        0..=9 => "just now".to_owned(),
        10..=59 => format!("{seconds} seconds ago"),
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

/// How long ago a file was last written, in words, or `None` when it cannot be read.
///
/// Used for the credentials file: a leftover secret carries no timestamp of its own,
/// but the file it lives in does, and that is the fact a person deciding whether to
/// delete it needs (C-4).
fn file_age(path: &std::path::Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let millis = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(tested_age(millis))
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
    ///
    /// The shape is one shape on every surface: `<login> <email> as `<agent>`
    /// (<mode>)`, with the creation time appended when the tool made the account
    /// (F6). The time is derived from the stored second, so the report ages instead
    /// of forever saying `created now` (H-3).
    pub fn describe(&self) -> String {
        match &self.result {
            Ok(identity) => {
                let line = identity.line();
                match identity.created_at {
                    Some(at) => {
                        format!("{line} (created {})", tested_age(at.saturating_mul(1000)))
                    }
                    None if identity.created => format!("{line} (created now)"),
                    None => line,
                }
            }
            Err(failure) => format!("failed ({}): {}", failure.class, failure.message),
        }
    }
}

/// A network check running on its own thread, so the screen can say it is in flight.
///
/// S-2: the endpoint test ran on the UI thread and the screen redrew only when it
/// returned, so for up to the probe bound the previous result kept being drawn as if
/// it were current and there was no sign the key had registered. Every network check
/// that displays a result — the endpoint test, the role check, the administrator's
/// role check and the identity — now runs here, behind an in-flight sentence, and the
/// result that lands is stamped with the time and the machine that made it (W32-2).
pub struct Probe {
    /// Which check is running, for the sentence drawn while it is in flight.
    pub what: ProbeWhat,
    /// When the thread started, in unix milliseconds.
    pub started_at_ms: u64,
    /// The machine that started it, when the mark can be read.
    pub machine: Option<String>,
    receiver: std::sync::mpsc::Receiver<ProbeDone>,
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Probe")
            .field("what", &self.what)
            .field("started_at_ms", &self.started_at_ms)
            .field("machine", &self.machine)
            .finish_non_exhaustive()
    }
}

/// Which check is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeWhat {
    /// The selected agent's endpoint.
    Endpoint { agent: String },
    /// The configured role's role check.
    Role,
    /// The administrator token's role check.
    AdminRole,
    /// The selected agent's identity.
    Identity { agent: String },
}

impl ProbeWhat {
    /// The in-flight sentence for this check, naming the machine when it is known.
    pub fn in_flight(&self, machine: Option<&str>) -> String {
        let here = match machine {
            Some(mark) => format!(" on {mark}"),
            None => String::new(),
        };
        match self {
            ProbeWhat::Endpoint { agent } => {
                format!("testing the endpoint of `{agent}`{here} — the result follows")
            }
            ProbeWhat::Role => {
                format!("checking the configured role{here} — the result follows")
            }
            ProbeWhat::AdminRole => {
                format!("checking the administrator role{here} — the result follows")
            }
            ProbeWhat::Identity { agent } => {
                format!("settling the identity of `{agent}`{here} — the result follows")
            }
        }
    }
}

/// The result a background thread sends back to the UI.
enum ProbeDone {
    Endpoint {
        agent: String,
        endpoint: String,
        model: String,
        result: Result<String, EndpointFailure>,
    },
    Role {
        admin: bool,
        url: String,
        result: Result<RoleCheck, ForgejoFailure>,
    },
    Identity {
        agent: String,
        result: Result<AgentIdentity, ForgejoFailure>,
    },
}

/// The one line the identity key writes to the status.
///
/// It is a receipt, not a second copy of the report: the report itself is drawn on
/// the Forgejo panel, once (W-3). Repeating it here printed the same sentence twice
/// on one 80×24 screen, the panel's copy clipped at the border. A **failure** keeps
/// its whole message: it names the key that fixes it (`v` on the Forgejo screen) and
/// the panel's copy can be below the fold.
fn identity_receipt(
    name: &str,
    result: &Result<crate::identity::AgentIdentity, ForgejoFailure>,
) -> String {
    match result {
        Ok(identity) => format!(
            "identity for `{name}` settled as `{}` ({}) — the Forgejo panel carries the report",
            identity.login,
            identity.mode.as_str()
        ),
        Err(failure) => format!(
            "identity for `{name}` failed ({}): {}",
            failure.class, failure.message
        ),
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

/// The status when an agent's own Forgejo token cannot be read.
///
/// A-2's class: a refusal the user acts on names the product's own keys for the fix
/// (`v` supplies the token, `i` settles the identity), never the credentials file and
/// its TOML table. Any other read failure keeps its own sentence.
fn missing_agent_token_status(name: &str, error: &credentials::CredentialsError) -> String {
    match error {
        credentials::CredentialsError::MissingToken { .. } => format!(
            "no Forgejo token for `{name}` — press v on the Forgejo screen to supply it, or i to settle the identity"
        ),
        _ => format!("no Forgejo token for `{name}`: {error}"),
    }
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
    /// Whether the acknowledgment is on screen in **review** mode: a second person
    /// asked to read the terms again with `R`, so no key records anything and every
    /// key simply closes it (C-3).
    pub ack_review: bool,
    /// The in-TUI add-agent form, when it is open.
    pub add_agent: Option<NewAgentForm>,
    /// A one-field prompt for a secret, when one is open.
    pub secret_prompt: Option<SecretPrompt>,
    /// A one-field prompt for the Forgejo address, when one is open (W32-1).
    pub url_prompt: Option<UrlPrompt>,
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
    /// The agent (and the reason) whose run record is present but cannot be read.
    ///
    /// A record that is not there is `forgotten`; a record that is there but
    /// unreadable is **not** the same fact, and the hub says which one it is (W38-1).
    pub unreadable_run: Option<(String, String)>,
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
    /// How many run records the configured agents have on disk, of which `recent`
    /// holds the newest [`RECENT_RUNS_LIMIT`]. The difference is what the list and the
    /// footer count must declare rather than hide (L-1, L-5).
    pub recent_total: usize,
    /// What every run record under the state root occupies, and how many belong to
    /// agents the configuration no longer lists (L-2, L-4).
    pub footprint: crate::run::RunFootprint,
    /// The result of the last "test this endpoint" action, if any, for this session.
    pub endpoint_test: Option<EndpointTest>,
    /// The endpoint tests read back from the record beside the acknowledgment file.
    ///
    /// LIB-2: a test's stamp vanished at restart, so the Agent screen drew no field at
    /// all. The record is what lets the stamp age across restarts; the session's own
    /// test above wins for the agent it names so a fresh result is shown at once.
    pub persisted_endpoint_tests: Vec<EndpointTest>,
    /// The network check in flight, if any.
    ///
    /// A check runs on its own thread so the screen can draw an in-flight sentence and
    /// never presents the previous result as the current one (S-2).
    pub probe: Option<Probe>,
    /// The result of the last role check, if any.
    pub role_check: Option<Result<RoleCheck, ForgejoFailure>>,
    /// When the last role check ran, in unix milliseconds (S-2's stamp).
    pub role_checked_at_ms: Option<u64>,
    /// The machine the last role check ran on, when the mark can be read.
    pub role_check_machine: Option<String>,
    /// The role-check stamp read back from the record, so the fact that a check ran
    /// survives a restart (W37-1).
    pub persisted_role_check_stamp: Option<RoleCheckStamp>,
    /// Which repository the Forgejo screen has selected.
    pub repo_selected: usize,
    /// The result of the last identity action, if any.
    pub identity_report: Option<IdentityReport>,
    /// What the selected agent's workspace holds, cached for the agent screen.
    ///
    /// A summary runs `git` several times, and the screen is redrawn many times a
    /// second, so it is recomputed at most once a second while the Agent screen is
    /// on show (W-2): the agent name it was read for, the sentence, and when.
    workspace_note: Option<(String, String, Instant)>,
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
    /// The width the body panels were last drawn into, so the scroll keys can count
    /// wrapped rows without the terminal size being threaded through every key
    /// (W36-1). Before the first frame it is the floor's own body width.
    pub panel_width: std::cell::Cell<usize>,
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
        // O2: a hand-off killed with `SIGKILL` cannot remove its own scratch root; the
        // next start of the product removes what a dead hand-off left.
        crate::editor::sweep_stale_scratch();
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
            ack_review: false,
            add_agent: None,
            secret_prompt: None,
            url_prompt: None,
            confirm_remove: None,
            confirm_forget: None,
            editor_confirm: None,
            editor_configured: crate::editor::editor_is_configured(),
            show_help: false,
            confirm_quit: None,
            help_offset: 0,
            panel_scroll: 0,
            forgotten_run: None,
            unreadable_run: None,
            config_note: None,
            screen: Screen::Agents,
            selected: 0,
            run_selected: 0,
            recent: Vec::new(),
            recent_total: 0,
            footprint: crate::run::RunFootprint::default(),
            endpoint_test: None,
            persisted_endpoint_tests: Vec::new(),
            probe: None,
            role_check: None,
            role_checked_at_ms: None,
            role_check_machine: None,
            persisted_role_check_stamp: None,
            repo_selected: 0,
            identity_report: None,
            workspace_note: None,
            start_failure: None,
            pending_stop: None,
            started_ms: crate::time::now_unix_ms(),
            forgejo_note: None,
            last_scan: None,
            panel_width: std::cell::Cell::new(70),
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
        // A screen change or an edit can change which workspace is on show; the
        // cache is keyed by agent name and a time, so the next tick re-reads it.
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
                // R-1: a tree an older version left keeps its old modes until the
                // product happens to write each path again. Hold the product's own
                // roots back to the one custody on every load, before anything is
                // read, so the mode the screen prints is the mode on disk.
                crate::mode::hold_tree_private(self.layout.root());
                crate::mode::hold_tree_private(self.layout.state_root());
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
                // LIB-2: the endpoint tests are a fact the product learned, not a
                // session's transient line. Read them back so the Agent screen can
                // draw the stamp (and its age) after a restart.
                self.persisted_endpoint_tests =
                    read_endpoint_tests(&self.layout.endpoint_tests_file());
                // W37-1: the role check's stamp is a fact the next start reads, so the
                // Forgejo screen can say when the last check ran instead of claiming
                // none ever did.
                self.persisted_role_check_stamp =
                    read_role_check_stamp(&self.layout.role_checks_file());
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
        // The reconciliation writes terminal records for abandoned runs; read them
        // back before seeding, so the hub and the Runs list cannot disagree for the
        // first second after a reopen about whether an abandoned run is still running
        // (O1).
        self.refresh_recent();
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
        self.unreadable_run = None;
        for name in names {
            // An outcome whose record is gone is dropped before seeding, so `r` after
            // deleting the run records puts the hub back in step with the ledger
            // instead of leaving it claiming an outcome no record holds (T-5).
            if let State::Finished { run_id, .. } = self.supervisor.state(&name) {
                // The run's own record is the truth, on disk and now: the in-memory
                // list may predate a terminal record written by the reconciliation
                // just above. A record that is gone takes its cached outcome with it;
                // a record that is present but unreadable is a different fact, and the
                // hub names it instead of pretending the run never ended (W38-1).
                let dir = self.layout.state_root().join(&name).join(&run_id);
                match crate::transcript::terminal_record_checked(&dir) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        self.supervisor.forget_state(&name);
                        // Say so once, and never let the next-oldest record pass
                        // silently for "the last run" (Y-4).
                        self.forgotten_run = Some(name.clone());
                    }
                    Err(source) => {
                        self.supervisor.forget_state(&name);
                        self.unreadable_run = Some((name.clone(), source.to_string()));
                    }
                }
            }
            if self.supervisor.state(&name) != State::Idle {
                continue;
            }
            let newest = self.recent.iter().find(|run| run.agent == name);
            // W41-2: a fresh start read a present-but-unreadable transcript as `idle`
            // — the word for "no run" — while the Runs screen said `unreadable`. The
            // newest record for the agent is on disk and its error is known; the hub
            // says the same word Runs does, and the note area gives the reason.
            if let Some(reason) = newest.and_then(|run| run.read_error.clone()) {
                let run_id = newest
                    .map(|run| run.run_id.clone())
                    .unwrap_or_default();
                self.unreadable_run = Some((name.clone(), reason.clone()));
                self.supervisor.note_unreadable(&name, run_id, reason);
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
        // R-5: the list is built from every agent directory on disk, not only the
        // configured ones, so a removed agent's records are listed (marked `*`) and
        // their transcript and log stay reachable. The count and the rows then agree.
        let mut listed = names.clone();
        listed.extend(crate::run::unconfigured_agents(
            self.layout.state_root(),
            &names,
        ));
        self.recent = recent_runs(self.layout.state_root(), &listed, RECENT_RUNS_LIMIT);
        // The cap is on the list, not on the records. Measure the records too, so the
        // screens can say how many older ones exist and how much room they take
        // (L-1, L-2, L-4): a list that stops at fifty without saying so is the finding.
        self.footprint = crate::run::state_footprint(self.layout.state_root(), &names);
        // `recent_total` is every record on disk now that the list covers every
        // record; the difference the cap clause declares is the cap, not a hidden
        // population (R-5).
        self.recent_total = self.footprint.records;
        self.last_scan = Some(Instant::now());
        if self.run_selected >= self.recent.len() {
            self.run_selected = self.recent.len().saturating_sub(1);
        }
    }

    /// The supervisor's live state for one run, when this UI is watching it now.
    ///
    /// A live run is the only state the two screens can disagree about: the hub reads
    /// the supervisor every frame, and the Runs list reads the records on a one-second
    /// timer (Z-1). Matching on the run id keeps a later run of the same agent from
    /// lending its state to an older row.
    fn live_state_for(&self, run: &crate::run::RunSummary) -> Option<State> {
        let state = self.supervisor.state(&run.agent);
        if state.is_live() && state.run_id() == Some(run.run_id.as_str()) {
            Some(state)
        } else {
            None
        }
    }

    /// The state word for one row of the Runs screen.
    ///
    /// A run this UI is watching right now takes the supervisor's own word — the
    /// same `State::word()` the hub prints — so the two screens cannot say `running`
    /// and `unknown start` about one run in flight (Z-1). A run the UI is not watching
    /// (an earlier session's, or one that ended while the UI was away) takes its word
    /// from its own record on disk.
    pub fn run_state_word(&self, run: &crate::run::RunSummary) -> String {
        match self.live_state_for(run) {
            Some(state) => state.word(),
            None => run.describe_at(crate::time::now_unix_ms()),
        }
    }

    /// The start clock for one row of the Runs screen.
    ///
    /// A run in flight may not have written its opening transcript record yet, so the
    /// record carries no start; its own id does, because the id is made from the
    /// instant the supervisor made it. Reading it there means the row has a start
    /// clock from the first frame, instead of `unknown start` beside a hub that
    /// already counts seconds (Z-1).
    pub fn run_started_text(&self, run: &crate::run::RunSummary) -> String {
        if self.live_state_for(run).is_some() {
            if let Some(ms) = crate::run::started_ms_from_run_id(&run.run_id) {
                return crate::time::compact_utc(ms);
            }
        }
        run.started_text()
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
        // The credentials file's own modification time is the closest thing to "when
        // did this leftover arrive": the secret carries no timestamp, the file does.
        let age = file_age(&self.credentials_path);
        crate::credentials::present_secret_names(&self.credentials_path)
            .into_iter()
            .map(|name| match backticked_agent(&name) {
                Some(agent) if !configured.contains(&agent.as_str()) => match &age {
                    Some(age) => format!(
                        "{name} (no agent with that name is configured; left by a credentials file last written {age})"
                    ),
                    None => format!("{name} (no agent with that name is configured)"),
                },
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

    /// Whether the footer's finished count cannot see every record that might count.
    ///
    /// The count is derived from the newest [`RECENT_RUNS_LIMIT`] records, so whenever
    /// older records exist on disk the true count may be larger. The footer then says
    /// `K+ finished` rather than presenting the window as exact — including when a run
    /// is in flight inside the window (W36-2, L-5).
    pub fn finished_count_is_capped(&self) -> bool {
        // The count is over the newest [`RECENT_RUNS_LIMIT`] records. Whenever any
        // older record exists on disk, that count is a lower bound, not an exact
        // total — even with a run in flight inside the window (W36-2).
        self.recent_total > self.recent.len()
    }

    /// A file's size, in the product's own words, or `None` when it cannot be read.
    pub fn file_size_text(path: &std::path::Path) -> Option<String> {
        let bytes = std::fs::metadata(path).ok()?.len();
        Some(filesize_text(bytes))
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

    /// The paths of one configured agent, reading the instruction the configuration
    /// names rather than the derived default.
    ///
    /// The instruction field is checked when the configuration is read, so the join
    /// can only reach inside the configuration directory. Every screen and every run
    /// goes through this, so they agree on the file (P-3).
    fn agent_paths(&self, name: &str) -> Result<crate::paths::AgentPaths, crate::paths::PathError> {
        let paths = self.layout.agent(name)?;
        match self.agents().iter().find(|agent| agent.name == name) {
            Some(agent) => Ok(paths.with_instruction(self.layout.root().join(&agent.instruction))),
            None => Ok(paths),
        }
    }

    /// The configured agent's paths, prepared on disk (including the instruction's
    /// parent directory, which a configured path may place elsewhere).
    fn ensure_agent_paths(
        &self,
        name: &str,
    ) -> Result<crate::paths::AgentPaths, crate::paths::PathError> {
        let paths = self.agent_paths(name)?;
        paths.ensure()?;
        Ok(paths)
    }

    /// The paths derived from the selected agent's name, when there is one.
    pub fn selected_paths(&self) -> Option<crate::paths::AgentPaths> {
        let name = self.selected_name()?;
        match self.agent_paths(&name) {
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
        // A review never records: it is a second person reading terms already agreed.
        // The confirm path is reached only from the first-run modal, which never has
        // `ack_review` set.
        self.ack_review = false;
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
        self.ack_review = false;
        self.status =
            "the risk was not acknowledged, so starting an agent is refused until it is recorded"
                .to_owned();
    }

    /// Show the risk acknowledgment again, without changing the record (C-3).
    ///
    /// A second person on a configuration directory someone else used can read what
    /// was agreed, when and under which version, and can read the terms themselves.
    /// When no record exists this is the ordinary first-run modal, which does record.
    pub fn review_acknowledgment(&mut self) {
        self.show_acknowledgment = true;
        if self.ack_state.is_confirmed() {
            self.ack_review = true;
            self.status =
                "the acknowledged terms are shown again; the record is not changed".to_owned();
        } else {
            self.ack_review = false;
        }
    }

    /// Close the review without touching the record.
    fn close_ack_review(&mut self) {
        self.show_acknowledgment = false;
        self.ack_review = false;
        self.status = "the terms were read; the record was not changed".to_owned();
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

    /// Whether an overlay is up and owns the plain keys.
    ///
    /// The footer's key legend is the product's own list. Under one of these overlays
    /// `q` is not `quit` — it is a letter in a field, a cancel, or a keep — so the
    /// legend must not advertise `q quit` there. The first-run acknowledgment is not
    /// in this set: `q` really does quit it, so its legend stays as it was (E-2).
    pub fn overlay_owns_keys(&self) -> bool {
        self.add_agent.is_some()
            || self.editor_confirm.is_some()
            || self.secret_prompt.is_some()
            || self.url_prompt.is_some()
            || self.confirm_remove.is_some()
            || self.confirm_forget.is_some()
            || self.show_help
            // A review of the acknowledgment owns every key too: `q` closes the
            // review there, it does not quit the product, so the footer must not
            // advertise `q quit` under it.
            || (self.show_acknowledgment && self.ack_review)
    }

    /// Whether an overlay is drawn over the body and so owns the note area behind it.
    ///
    /// F-1's class, generalized by enumeration: a bordered overlay can cover the head
    /// of a wrapped note-area sentence and leave its tail (`writes both files for
    /// you.`) as the first thing a zero-knowledge user reads. The acknowledgment is
    /// the topmost overlay and is not in `overlay_owns_keys` (it does not own `q`),
    /// so it is added here; every other overlay is the same set. One predicate governs
    /// the note area, and a new overlay added to `overlay_owns_keys` is covered
    /// without a second list.
    pub fn overlay_owns_screen(&self) -> bool {
        self.overlay_owns_keys() || self.show_acknowledgment
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
        // W32-1: the form's address field writes the **global** `[forgejo] url`. When
        // it differs from the address in force and other agents exist, the form moved
        // every one of them, so its receipt must say so rather than hide it.
        let moved_address = self
            .config
            .as_ref()
            .filter(|config| !config.agents.is_empty())
            .filter(|config| {
                let typed = form.fields[4].trim();
                !typed.is_empty() && typed != config.forgejo.url
            })
            .map(|config| config.forgejo.url.clone());
        match self.write_new_agent(&form) {
            Ok(name) => {
                self.add_agent = None;
                self.reload();
                self.status = match moved_address {
                    Some(previous) => format!(
                        "created `{name}`: write its instruction with e, then press s to start it — the form also moved the Forgejo address from {previous} for every agent in this configuration; u on the Forgejo screen edits it on its own"
                    ),
                    None => format!(
                        "created `{name}`: write its instruction with e, then press s to start it"
                    ),
                };
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
        crate::mode::write_private(self.config_path(), text.as_bytes()).map_err(|error| {
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
        if let Err(error) = crate::mode::write_private(self.config_path(), text.as_bytes()) {
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
            // F-2's sweep: every refusal replaces the optimistic status, so the
            // footer and the prompt hold the same answer.
            self.status = format!(
                "not written: no {} was typed, and an empty secret is refused",
                prompt.target.describe()
            );
            if let Some(open) = self.secret_prompt.as_mut() {
                open.error = Some("a secret cannot be empty".to_owned());
            }
            return;
        }
        // W37-2: the role token and the administrator token are stored globally and
        // act as the connection, whatever agent happens to be selected. Only a
        // per-agent secret is attributed to an agent, so the sentence never reports a
        // global token "for `scribe`".
        let subject = if prompt.target.needs_agent() {
            prompt
                .agent
                .clone()
                .or_else(|| self.selected_name())
                .unwrap_or_else(|| "the connection".to_owned())
        } else {
            "the connection".to_owned()
        };
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
                // F-2: a refusal replaces the optimistic status, not sits under it.
                // The footer and the prompt hold the same sentence, so a person who
                // asks "did it get written?" is answered on the line they look at.
                let refusal = error.to_string();
                self.status = format!(
                    "not written: the {} for {subject} was refused — {refusal}",
                    prompt.target.describe()
                );
                if let Some(open) = self.secret_prompt.as_mut() {
                    open.error = Some(refusal);
                }
            }
        }
    }

    /// Open the one-field prompt that edits the configured Forgejo address (W32-1).
    ///
    /// The address was settable only by adding an agent, whose writer silently moved
    /// the **global** `[forgejo] url` for every agent that already existed. It has its
    /// own place now: `u` on the Forgejo screen, prefilled with the address in force,
    /// and its own sentence saying that every agent in this configuration follows it.
    pub fn request_url_edit(&mut self) {
        let Some(settings) = self.forgejo_settings() else {
            self.status = "the configuration could not be read, so the address cannot be edited"
                .to_owned();
            return;
        };
        self.status =
            "type the Forgejo address and press enter; it applies to every agent in this configuration"
                .to_owned();
        self.url_prompt = Some(UrlPrompt {
            value: settings.url,
            error: None,
        });
    }

    /// One key while the address prompt is open.
    fn on_url_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit_url(),
            KeyCode::Esc => {
                self.url_prompt = None;
                self.status = "the Forgejo address was not changed".to_owned();
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.url_prompt.as_mut() {
                    prompt.value.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(prompt) = self.url_prompt.as_mut() {
                    prompt.value.push(character);
                }
            }
            _ => {}
        }
    }

    /// Write the address the prompt holds, preserving every other field.
    fn submit_url(&mut self) {
        let Some(prompt) = self.url_prompt.clone() else {
            return;
        };
        let url = prompt.value.trim().to_owned();
        if url.is_empty() {
            self.status = "not written: the Forgejo address cannot be empty".to_owned();
            if let Some(open) = self.url_prompt.as_mut() {
                open.error = Some("the address cannot be empty".to_owned());
            }
            return;
        }
        let Some(existing) = self.config.clone() else {
            self.status = format!(
                "the configuration file cannot be read, so the address cannot be written; fix {} first",
                self.config_path().display()
            );
            return;
        };
        let mut updated = existing.clone();
        updated.forgejo.url = url.clone();
        let text = render_full_config(&updated, &updated.agents);
        if let Err(error) = Config::from_text(&self.config_path(), &text) {
            self.status = error.to_string();
            if let Some(open) = self.url_prompt.as_mut() {
                open.error = Some(error.to_string());
            }
            return;
        }
        if let Err(error) = crate::mode::write_private(self.config_path(), text.as_bytes()) {
            let message = format!("{} cannot be written: {error}", self.config_path().display());
            self.status = message.clone();
            if let Some(open) = self.url_prompt.as_mut() {
                open.error = Some(message);
            }
            return;
        }
        self.url_prompt = None;
        self.reload();
        // The old address may have been another machine's instance; the view from it
        // is not this address's view, so no stale repository list is kept. The new
        // address must be checked on its own (`c`).
        self.role_check = None;
        self.repo_selected = 0;
        self.status = format!(
            "the Forgejo address is now {url} — it applies to every agent in this configuration; press c to check the role against it"
        );
    }

    /// Ask to leave. If runs are in flight, name what leaving ends and ask again.
    pub fn request_quit(&mut self) {
        self.ask_to_leave("q");
    }

    /// Ask to leave after a hand-off caught a `^C`.
    ///
    /// The UI is not in raw mode while an editor or a pager has the terminal, so
    /// `^C` there is a `SIGINT`; the product catches it and, when the hand-off's own
    /// exit returns, takes the same leave path the TUI's `^C` takes: one warning that
    /// names the runs when work is in flight, and an immediate, non-destructive leave
    /// when none is (E-3).
    pub fn request_leave_after_handoff(&mut self) {
        self.ask_to_leave("ctrl-c");
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

    /// A run record's reason, read against the configuration as it is now.
    ///
    /// A record's reason is history: the paths in it were the ones in force when that
    /// run ran. When the configuration directory has since moved — a copied `HOME`, a
    /// renamed installation — the reason names a file that is not the one on this
    /// screen, and a reader takes it for today's (W-1). When the reason embeds a path
    /// that is not the live instruction, the drawn sentence says the path is the
    /// run's own and names the file the configuration points at now. Nothing is
    /// claimed about whether the run's complaint still holds.
    pub fn reason_with_history(&self, agent: &str, reason: &str) -> String {
        let text = crate::model::in_product_words(reason);
        let live = self
            .agent_paths(agent)
            .ok()
            .map(|paths| paths.instruction().display().to_string());
        match live {
            Some(live) => match foreign_path(&text, &live) {
                Some(path) => format!(
                    "{text} (the path `{path}` is from when that run ran; the instruction now is {live})"
                ),
                None => text,
            },
            None => text,
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
        let paths = match self.ensure_agent_paths(&name) {
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
            // Lead with the fix, and name the product's own key for it: the screen
            // that sets a model key is one keypress away, so the refusal must not send
            // the user to a file and a TOML section instead (A-2). The run's own log
            // still carries the file's full sentence.
            self.status = format!(
                "not started: no model key for `{name}` — press 5 and then k on the Credentials screen to add one"
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
        if let Some(sentence) = self.probe_sentence() {
            self.status = format!("{sentence}; the previous check is still in flight");
            return;
        }
        // S-2: the probe runs on its own thread, so the screen can draw the in-flight
        // sentence and never shows the previous result as the current one. The key is
        // read, the request made and the result classified off the UI thread; the
        // result is applied in `finish_probe` on the next tick.
        let credentials = self.layout.credentials_file();
        let name = agent.name.clone();
        let endpoint = agent.endpoint.clone();
        let model_id = agent.model.clone();
        self.start_probe(
            ProbeWhat::Endpoint {
                agent: name.clone(),
            },
            move || {
                let result = match credentials::model_key(&credentials, &name) {
                    Err(error) => Err(EndpointFailure {
                        class: "missing_key".to_owned(),
                        message: error.to_string(),
                    }),
                    Ok(key) => {
                        let config = model::ModelConfig {
                            endpoint: endpoint.clone(),
                            model: model_id.clone(),
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
                ProbeDone::Endpoint {
                    agent: name,
                    endpoint,
                    model: model_id,
                    result,
                }
            },
        );
        self.status = self.probe_sentence().unwrap_or_default();
    }

    /// The endpoint test the Agent screen knows for one agent.
    ///
    /// The session's own test wins for the agent it names, so a fresh result is shown
    /// the moment it is made; otherwise the test read back from the record is used, so
    /// a stamp survives a restart and ages (LIB-2).
    pub fn endpoint_test_for(&self, agent: &str) -> Option<&EndpointTest> {
        if let Some(test) = &self.endpoint_test {
            if test.agent == agent {
                return Some(test);
            }
        }
        self.persisted_endpoint_tests
            .iter()
            .find(|test| test.agent == agent)
    }

    /// The sentence for the network check in flight, when one is.
    pub fn probe_sentence(&self) -> Option<String> {
        self.probe
            .as_ref()
            .map(|probe| probe.what.in_flight(probe.machine.as_deref()))
    }

    /// The in-flight sentence for this agent's endpoint test, when one is running.
    ///
    /// The field says a test is under way and when it started, so the result that is
    /// still on screen from the last probe is never read as the current one (S-2).
    pub fn endpoint_probe_sentence(&self, agent: &str) -> Option<String> {
        let started = self.endpoint_probe_started_at(agent)?;
        Some(format!(
            "testing — started {} (the result will replace this)",
            tested_age(started)
        ))
    }

    /// Whether this agent's endpoint is being tested right now (S-2).
    pub fn testing_endpoint(&self, agent: &str) -> bool {
        matches!(
            &self.probe,
            Some(probe)
                if matches!(&probe.what, ProbeWhat::Endpoint { agent: name } if name == agent)
        )
    }

    /// When the in-flight endpoint test for this agent started, in unix milliseconds.
    pub fn endpoint_probe_started_at(&self, agent: &str) -> Option<u64> {
        match &self.probe {
            Some(probe) => match &probe.what {
                ProbeWhat::Endpoint { agent: name } if name == agent => Some(probe.started_at_ms),
                _ => None,
            },
            None => None,
        }
    }

    /// Start a network check on its own thread, replacing no running one.
    ///
    /// The caller has already refused when a check is in flight; the thread only
    /// computes and sends, and [`App::poll_probe`] applies the result on the UI thread
    /// so the state changes in one place.
    fn start_probe(&mut self, what: ProbeWhat, work: impl FnOnce() -> ProbeDone + Send + 'static) {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        self.probe = Some(Probe {
            what,
            started_at_ms: crate::time::now_unix_ms(),
            machine: crate::machine::mark(),
            receiver,
        });
    }

    /// Apply a finished network check, if one has finished since the last tick.
    fn poll_probe(&mut self) {
        let done = match &self.probe {
            Some(probe) => match probe.receiver.try_recv() {
                Ok(done) => Some(done),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The thread ended without a result. Say so instead of leaving the
                    // in-flight sentence standing for ever.
                    self.status =
                        "the check ended without a result; press the key again to retry".to_owned();
                    self.probe = None;
                    None
                }
            },
            None => None,
        };
        if let Some(done) = done {
            self.probe = None;
            self.finish_probe(done);
        }
    }

    /// Wait for the check in flight to finish, applying it on this thread.
    ///
    /// Used by the tests, which drive a check and then read the sentence it draws; the
    /// real terminal loop lets [`App::tick`] apply the result on the next frame.
    pub fn wait_for_probe(&mut self) {
        while self.probe.is_some() {
            self.poll_probe();
            if self.probe.is_some() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Apply one finished check on the UI thread.
    fn finish_probe(&mut self, done: ProbeDone) {
        match done {
            ProbeDone::Endpoint {
                agent,
                endpoint,
                model,
                result,
            } => {
                let short = match &result {
                    Ok(_) => "ok".to_owned(),
                    Err(_) => describe_endpoint_result(&result),
                };
                self.status = format!("endpoint test for `{agent}`: {short}");
                let test = EndpointTest {
                    agent,
                    endpoint,
                    model,
                    result,
                    tested_at_ms: crate::time::now_unix_ms(),
                    // W32-2: the result is written with the machine that made it.
                    machine: crate::machine::mark(),
                };
                // LIB-2: write the result down before showing it, so the stamp is a
                // fact the next start can read (and age). A failure to save is said on
                // the same status line rather than hidden.
                match write_endpoint_test(&self.layout.endpoint_tests_file(), &test) {
                    Ok(()) => {
                        self.persisted_endpoint_tests =
                            read_endpoint_tests(&self.layout.endpoint_tests_file());
                    }
                    Err(error) => {
                        self.status = format!(
                            "{} — the result could not be saved for the next start: {error}",
                            self.status
                        );
                    }
                }
                self.endpoint_test = Some(test);
            }
            ProbeDone::Role { admin, url, result } => {
                if admin {
                    self.apply_admin_role_check(result);
                } else {
                    self.apply_role_check(url, result);
                }
            }
            ProbeDone::Identity { agent, result } => {
                self.status = identity_receipt(&agent, &result);
                self.identity_report = Some(IdentityReport { agent, result });
            }
        }
    }

    /// The stamp for the role check's result, so a view cannot read as current forever
    /// (S-2).
    pub fn role_check_stamp(&self) -> Option<String> {
        // W37-1: a session that has run a check uses its own fields; a fresh session
        // uses the stamp read back from the record, so the age survives a restart and
        // the note's word `persisted` is true.
        let (at, machine) = match self.role_checked_at_ms {
            Some(at) => (at, self.role_check_machine.clone()),
            None => {
                let stamp = self.persisted_role_check_stamp.as_ref()?;
                (stamp.checked_at_ms, stamp.machine.clone())
            }
        };
        Some(match &machine {
            Some(mark) => format!("checked {} on {mark}", tested_age(at)),
            None => format!(
                "checked {}; this machine's own name cannot be read",
                tested_age(at)
            ),
        })
    }

    /// Write the role check's stamp down beside the endpoint-test record (W37-1).
    fn persist_role_check_stamp(&mut self, at: u64, machine: Option<String>) {
        let stamp = RoleCheckStamp {
            checked_at_ms: at,
            machine,
        };
        if let Err(error) = write_role_check_stamp(&self.layout.role_checks_file(), &stamp) {
            // Say the stamp could not be saved on the status line rather than hiding
            // it: the next start's honesty depends on the file.
            self.status = format!(
                "{} — the check's stamp could not be saved for the next start: {error}",
                self.status
            );
        }
        self.persisted_role_check_stamp = Some(stamp);
    }

    /// Apply the role check's result, in the sentence the Forgejo screen made before.
    fn apply_role_check(&mut self, url: String, result: Result<RoleCheck, ForgejoFailure>) {
        let at = crate::time::now_unix_ms();
        let mark = crate::machine::mark();
        self.role_checked_at_ms = Some(at);
        self.role_check_machine = mark.clone();
        self.status = match &result {
            Ok(check) => format!("role check: {}", check.summary()),
            // The configured URL leads the refusal: a mistyped path is the common
            // cause, and the API path alone does not connect the failure to what the
            // user typed (P-7).
            Err(failure) => format!(
                "role check failed ({}) at {}: {}",
                failure.class, url, failure.message
            ),
        };
        self.role_check = Some(result);
        self.repo_selected = 0;
        self.persist_role_check_stamp(at, mark);
    }

    /// Apply the administrator role check's result, keeping the last good view.
    fn apply_admin_role_check(&mut self, result: Result<RoleCheck, ForgejoFailure>) {
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
                let at = crate::time::now_unix_ms();
                let mark = crate::machine::mark();
                self.role_check = Some(Ok(check));
                self.role_checked_at_ms = Some(at);
                self.role_check_machine = mark.clone();
                self.repo_selected = 0;
                self.persist_role_check_stamp(at, mark);
            }
            Err(failure) => {
                let keep = matches!(self.role_check, Some(Ok(_)));
                let message = format!(
                    "role check with the administrator token failed ({}): {}",
                    failure.class, failure.message
                );
                if keep {
                    // The kept view keeps its own stamp: the failed check is about the
                    // administrator token, not the view on screen.
                    self.forgejo_note = Some(message);
                } else {
                    let at = crate::time::now_unix_ms();
                    let mark = crate::machine::mark();
                    self.role_check = Some(Err(failure));
                    self.role_checked_at_ms = Some(at);
                    self.role_check_machine = mark.clone();
                    self.repo_selected = 0;
                    self.persist_role_check_stamp(at, mark);
                }
            }
        }
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
        if let Some(sentence) = self.probe_sentence() {
            self.status = format!("{sentence}; the previous check is still in flight");
            return;
        }
        let role = settings.role;
        let url = settings.url.clone();
        // The token read is local and fast; the network call is what runs on the
        // background thread, behind the in-flight sentence (S-2).
        let token = match identity::role_token(&self.credentials_path, role) {
            Ok(token) => token,
            Err(error) => {
                self.apply_role_check(
                    url,
                    Err(ForgejoFailure {
                        class: error.class().to_owned(),
                        message: error.to_string(),
                    }),
                );
                return;
            }
        };
        let config_url = url.clone();
        self.start_probe(ProbeWhat::Role, move || {
            let config = forgejo::ForgejoConfig {
                url: config_url,
                token,
                timeout: forgejo::DEFAULT_TIMEOUT,
            };
            let result = forgejo::role_check(&config, role).map_err(failure_of);
            ProbeDone::Role {
                admin: false,
                url,
                result,
            }
        });
        self.status = self.probe_sentence().unwrap_or_default();
    }

    /// Run the role check with the administrator token, so both roles can be compared
    /// from inside the app instead of by editing the credentials file and restarting.
    pub fn request_admin_role_check(&mut self) {
        let Some(settings) = self.forgejo_settings() else {
            self.status = "the configuration could not be read, so no Forgejo can be checked"
                .to_owned();
            return;
        };
        if let Some(sentence) = self.probe_sentence() {
            self.status = format!("{sentence}; the previous check is still in flight");
            return;
        }
        let url = settings.url.clone();
        match credentials::forgejo_admin_token(&self.credentials_path) {
            Ok(Some(token)) => {
                self.start_probe(ProbeWhat::AdminRole, move || {
                    let config = forgejo::ForgejoConfig {
                        url: url.clone(),
                        token,
                        timeout: forgejo::DEFAULT_TIMEOUT,
                    };
                    let result = forgejo::role_check(&config, Role::Admin).map_err(failure_of);
                    ProbeDone::Role {
                        admin: true,
                        url,
                        result,
                    }
                });
                self.status = self.probe_sentence().unwrap_or_default();
            }
            Ok(None) => self.apply_admin_role_check(Err(ForgejoFailure {
                class: "missing_admin_token".to_owned(),
                // V-1: the refusal a person acts on names the product's own key for the
                // fix. The old sentence named the credentials file and no key, so the
                // worst reading was to edit the file by hand — the class that had
                // recurred four times. `A` on the Credentials screen is what writes
                // `forgejo_admin_token`, and it is the same key the sibling
                // `IdentityError::NoAdminToken` names.
                message: "no administrator token is configured, so only the configured \
                          role can be checked — press A on the Credentials screen to add one"
                    .to_owned(),
            })),
            Err(error) => self.apply_admin_role_check(Err(ForgejoFailure {
                class: "credentials".to_owned(),
                message: error.to_string(),
            })),
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
        if let Some(sentence) = self.probe_sentence() {
            self.status = format!("{sentence}; the previous check is still in flight");
            return;
        }
        let url = settings.url.clone();
        let credentials = self.credentials_path.clone();
        self.start_probe(
            ProbeWhat::Identity {
                agent: name.clone(),
            },
            move || {
                let result =
                    identity::resolve(&url, &name, &credentials).map_err(|error| ForgejoFailure {
                        class: error.class().to_owned(),
                        message: crate::model::in_product_words(&error.to_string()),
                    });
                ProbeDone::Identity {
                    agent: name,
                    result,
                }
            },
        );
        self.status = self.probe_sentence().unwrap_or_default();
    }

    /// The identity recorded for an agent, in the one shape every surface uses.
    ///
    /// Read from the credentials file rather than from a session's transient report,
    /// so a second person who restarts the product still sees whose identity is
    /// settled (F1, F6, H-3).
    pub fn recorded_identity(&self, agent: &str) -> Option<crate::identity::AgentIdentity> {
        let login = credentials::recorded_login(&self.credentials_path, agent)
            .ok()
            .flatten()?;
        // W2-A's sibling on the identity line: an email that was never recorded is
        // not an empty one. Without both, the identity is not settled.
        let email = credentials::recorded_email(&self.credentials_path, agent)
            .ok()
            .flatten()?;
        let mode = identity::recorded_mode(&self.credentials_path)
            .ok()
            .flatten();
        let created_at = credentials::recorded_identity_created(&self.credentials_path, agent)
            .ok()
            .flatten();
        Some(crate::identity::AgentIdentity {
            agent: agent.to_owned(),
            login,
            email,
            mode: mode.unwrap_or(identity::IdentityMode::Supplied),
            created: created_at.is_some(),
            created_at,
        })
    }

    /// The identity report a surface should draw for one agent.
    ///
    /// The session's own report wins when it is about this agent (it may carry a
    /// refusal). Otherwise, once the role check has run and an identity is recorded,
    /// the report is rebuilt from the credentials file, so it survives a restart the
    /// way the page map's `checked` row requires (H-3).
    pub fn identity_report_for(&self, agent: &str) -> Option<IdentityReport> {
        if let Some(report) = &self.identity_report {
            if report.agent == agent {
                return Some(report.clone());
            }
        }
        if !matches!(self.role_check, Some(Ok(_))) {
            return None;
        }
        let identity = self.recorded_identity(agent)?;
        Some(IdentityReport {
            agent: agent.to_owned(),
            result: Ok(identity),
        })
    }

    /// What the selected agent's workspace holds, when it has been read.
    pub fn workspace_note(&self) -> Option<&str> {
        let name = self.selected_name()?;
        match &self.workspace_note {
            Some((cached, note, _)) if cached == &name => Some(note.as_str()),
            _ => None,
        }
    }

    /// Read the selected agent's workspace once a second while its screen is on show.
    fn refresh_workspace_note(&mut self) {
        if self.screen != Screen::AgentConfig {
            return;
        }
        let Some(name) = self.selected_name() else {
            return;
        };
        let stale = match &self.workspace_note {
            Some((cached, _, at)) => cached != &name || at.elapsed() >= Duration::from_secs(1),
            None => true,
        };
        if stale {
            let note = self.workspace_note_for(&name);
            self.workspace_note = Some((name, note, Instant::now()));
        }
    }

    /// One sentence about the repository in an agent's workspace (W-2).
    ///
    /// What is knowable is read from the repository itself; what the product cannot
    /// read is said plainly instead of guessed.
    fn workspace_note_for(&self, name: &str) -> String {
        let paths = match self.agent_paths(name) {
            Ok(paths) => paths,
            Err(_) => return "the workspace path cannot be decided".to_owned(),
        };
        let workspace = paths.workspace();
        if !workspace.exists() {
            return "the workspace directory is not there yet; a run or a clone creates it"
                .to_owned();
        }
        let state = match workspace::inspect(workspace) {
            Ok(Some(state)) => state,
            Ok(None) => {
                // R-2: the product's own clone lands at `workspace/<repo>`, so after a
                // successful clone the root is still not a repository. Reading that as
                // “not a git repository” contradicted the clone that just happened; the
                // clones below the root are named instead.
                let clones = workspace::cloned_repositories(workspace);
                if clones.is_empty() {
                    return "not a git repository — the product reads it as a directory, not as \
                            history"
                        .to_owned();
                }
                return self.cloned_workspace_note(&clones);
            }
            Err(workspace::WorkspaceError::GitMissing { .. }) => {
                return "git is not installed, so the product cannot read what is in this directory"
                    .to_owned();
            }
            Err(error) => return format!("the repository could not be read ({})", error.detail()),
        };
        // W2-A: the product can only compare the repository's identity against an
        // identity the credentials file actually records. Building one out of two
        // empty strings made every repository read as "not the agent's" before any
        // identity was settled, a claim the product cannot support.
        let recorded = match (
            credentials::recorded_login(&self.credentials_path, name)
                .ok()
                .flatten(),
            credentials::recorded_email(&self.credentials_path, name)
                .ok()
                .flatten(),
        ) {
            (Some(login), Some(email)) => Some(format!("{login} <{email}>")),
            _ => None,
        };
        let whose = match (&state.local_identity, &recorded) {
            (Some(identity), Some(agent_identity)) if identity == agent_identity => {
                // N-2: the login and its email are one fact; the space between them
                // is drawn as a no-break space so a wrap cannot eat it and leave the
                // email glued to the login. The stored value stays ASCII for the
                // comparison above.
                let shown = words::join_identity(identity);
                format!("the local git identity is the agent's ({shown})")
            }
            (Some(identity), Some(_)) => {
                let shown = words::join_identity(identity);
                format!("the local git identity is {shown}, not the agent's")
            }
            (Some(identity), None) => {
                let shown = words::join_identity(identity);
                format!(
                    "the local git identity is {shown}; no Forgejo identity is recorded for this agent yet, so the product cannot say whether it is the agent's — press i to settle one"
                )
            }
            (None, _) => "no local git identity is set".to_owned(),
        };
        let changed = match state.changed {
            0 => "clean".to_owned(),
            1 => "1 changed path".to_owned(),
            count => format!("{count} changed paths"),
        };
        if state.commit.is_empty() {
            return format!("an empty repository on {} (no commit yet), {changed} — {whose}", state.branch);
        }
        let age = tested_age(state.committed_at.saturating_mul(1000));
        format!(
            "on {}, {} ({age}), {changed} — {whose}",
            state.branch, state.commit
        )
    }

    /// One sentence about the repositories cloned below a workspace root (R-2).
    ///
    /// Each clone is named with what the product can read of it — branch, commit and
    /// age, and whether the tree is clean — so the clone the product itself made is
    /// visible on the screen that used to call the root “not a git repository”.
    fn cloned_workspace_note(&self, clones: &[workspace::ClonedRepository]) -> String {
        let listed: Vec<String> = clones
            .iter()
            .map(|clone| match &clone.state {
                Some(state) => {
                    let changed = match state.changed {
                        0 => "clean".to_owned(),
                        1 => "1 changed path".to_owned(),
                        count => format!("{count} changed paths"),
                    };
                    if state.commit.is_empty() {
                        format!(
                            "{} (on {} (no commit yet), {changed})",
                            clone.name, state.branch
                        )
                    } else {
                        let age = tested_age(state.committed_at.saturating_mul(1000));
                        format!(
                            "{} (on {}, {} ({age}), {changed})",
                            clone.name, state.branch, state.commit
                        )
                    }
                }
                None => format!("{} (the repository could not be read)", clone.name),
            })
            .collect();
        let noun = if clones.len() == 1 {
            "repository"
        } else {
            "repositories"
        };
        format!(
            "not a git repository; it holds {} cloned {noun}: {}",
            clones.len(),
            listed.join(", ")
        )
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
                    "no Forgejo identity is recorded for `{agent}`; press i to settle one, or press v on this screen to supply its token"
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
            Ok((path, outcome)) => {
                // Say what actually happened: a real clone and the reuse of a
                // repository already on the ground are different acts, and calling
                // both "is ready" hides the second — including the fact that its
                // local git identity was just written (F2).
                let note = match outcome {
                    workspace::Prepared::Cloned => format!(
                        "{} was cloned into {} as {}",
                        repository.full_name,
                        path.display(),
                        identity.line()
                    ),
                    workspace::Prepared::AlreadyPresent => format!(
                        "{} was already a git repository at {} — kept, not cloned or fetched; its local git identity was set to {}",
                        repository.full_name,
                        path.display(),
                        identity.line()
                    ),
                };
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
                self.status = missing_agent_token_status(&name, &error);
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
                self.status = missing_agent_token_status(&name, &error);
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
        let paths = match self.ensure_agent_paths(&name) {
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
        // The file is **not** created here: a person who backs out of the
        // confirmation, or whose editor fails to start, must not be left with a file
        // the product made for them. It is written at the hand-off, and the status
        // then says so instead of claiming nothing happened (H-1, H-2).
        let template = editable_template(target, &name);
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
        let action = PendingAction {
            kind: ActionKind::Edit,
            path,
            label: format!("the {} of `{name}`", target.label()),
            before: None,
            target: Some(target),
            created: false,
            template: Some(template.into_bytes()),
        };
        // N-1: the program is named **before** it takes the screen in both cases. A
        // status line is erased the moment the UI steps aside for the editor, so both
        // paths wait on a confirmation that names the program (and, for the product's
        // own fallback editor, says how to leave it). The confirmation is also the
        // one place a person can back out before anything is written (H-1, H-2).
        let program = crate::editor::editor();
        self.editor_confirm = Some(action);
        if self.editor_configured {
            self.status = format!(
                "`$EDITOR` is set: press enter to open the {} of `{name}` in {program}{}",
                target.label(),
                recovery.as_deref().unwrap_or("")
            );
        } else {
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
                    self.status = format!("`{name}` has no run record on this machine yet");
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
            target: None,
            created: false,
            template: None,
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
                    self.status = format!("`{name}` has no run record on this machine yet");
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
            target: None,
            created: false,
            template: None,
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

    /// The newest run summary for one agent, when the list holds one.
    ///
    /// A screen that must say *why* a file cannot be shown — gone, or present but
    /// unreadable — reads the run summary rather than the path alone (W38-1).
    pub fn latest_run(&self, name: &str) -> Option<&RunSummary> {
        self.recent.iter().find(|run| run.agent == name)
    }

    /// The log file of the newest run of one agent, for a screen to name.
    pub fn latest_run_log(&self, name: &str) -> Option<PathBuf> {
        self.latest_run_dir(name)
            .map(|dir| dir.join(crate::run::LOG_FILE))
    }

    /// The transcript file of the newest run of one agent, for a screen to name.
    pub fn latest_run_transcript(&self, name: &str) -> Option<PathBuf> {
        self.latest_run_dir(name)
            .map(|dir| dir.join(crate::transcript::TRANSCRIPT_FILE))
    }

    /// Take any work the terminal loop has to do, leaving none behind.
    ///
    /// A file an edit will open is written **here**, at the moment of the hand-off,
    /// not when the key was pressed: a person who cancels the confirmation is left
    /// with nothing, and a hand-off that does create a file can say so truthfully
    /// (H-1, H-2).
    pub fn take_action(&mut self) -> Option<PendingAction> {
        let mut action = self.pending.take()?;
        if action.kind == ActionKind::Edit {
            if let Some(template) = action.template.take() {
                if !action.path.exists() {
                    if let Err(error) = write_template(&action.path, &template) {
                        self.status = format!("{} cannot be prepared: {error}", action.label);
                        return None;
                    }
                    action.created = true;
                }
            }
            // Fingerprint the file as the editor will first see it, *after* any
            // template was written, so an untouched template is told from an edit.
            action.before = stamp(&action.path);
        }
        Some(action)
    }

    /// Say how an edit or a view ended.
    pub fn note_action(
        &mut self,
        action: &PendingAction,
        result: std::io::Result<std::process::ExitStatus>,
    ) {
        // T-2: the hand-off program owned the keys while it was on screen. When it
        // returns, the product owns them again; every hand-off outcome says so, so a
        // habitual editor key (the `N` a beginner types after `^X`) is not sent to the
        // product without the person being told the hand-over happened.
        const KEYS_BACK: &str = "; the keys are the product's again";
        match (action.kind, result) {
            (ActionKind::Edit, Ok(status)) => {
                // Compare the file before and after: never claim an edit that was not
                // written. A person who quit the editor without saving must be told so.
                let changed = action.before != stamp(&action.path);
                let program = crate::editor::editor();
                // W-4: the sentence names the thing (`the declared steps`) and now the
                // file too, so the status line alone is enough to find what was
                // created or opened.
                let file = action
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("the file");
                self.reload();
                if !status.success() {
                    // The editor may have failed before saving; if the hand-off itself
                    // created the file, say so too, so the count on the screen beside
                    // it is never unexplained (H-1, H-2).
                    let created = if action.created {
                        "; the template was created and left as it was"
                    } else {
                        ""
                    };
                    // R-4: `code None` was a Rust `Option` on a screen, the emergency
                    // save the product had just detected was not named, and the return
                    // clause every other hand-off outcome carries was missing. The
                    // interruption now reads as a sentence, names the file to recover
                    // from where it happened, and says the keys are the product's again.
                    let ending = match status.code() {
                        Some(code) => format!("left with code {code}"),
                        None => "was interrupted".to_owned(),
                    };
                    let recovery = match editor_backup(&action.path) {
                        Some(backup) => format!(
                            "; an earlier edit was not saved and the editor kept it in {} — open that file to recover",
                            backup.display()
                        ),
                        None => String::new(),
                    };
                    self.status = format!(
                        "{program} {ending} for {} ({file}); it may not have been saved{created}{recovery}{KEYS_BACK}",
                        action.label
                    );
                } else {
                    // `declared steps` is plural; `instruction` and `new skill` are not.
                    let verb = if action.target.map(EditTarget::is_plural).unwrap_or(false) {
                        "were"
                    } else {
                        "was"
                    };
                    if changed {
                        self.status = format!(
                            "{} {verb} edited in {program} ({file}); the files were read again{KEYS_BACK}",
                            action.label
                        );
                    } else if action.created {
                        // The hand-off itself made the file; the screen beside it now
                        // counts it. Saying `not changed` here is the lie H-1 and H-2
                        // found: report the creation instead.
                        self.status = format!("{}{KEYS_BACK}", created_status(action, &program));
                    } else {
                        self.status = format!(
                            "{} {verb} not changed ({file}, opened in {program}){KEYS_BACK}",
                            action.label
                        );
                    }
                }
            }
            (ActionKind::Edit, Err(error)) => {
                let created = if action.created {
                    "; the template was created and is unchanged"
                } else {
                    ""
                };
                self.status =
                    format!("your editor could not be started: {error}{created}");
            }
            (ActionKind::View, Ok(_)) => {
                self.status = format!("{} was shown{KEYS_BACK}", action.label);
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
    ///
    /// Three cases, three answers (Z-3, W38-1's class): the file is there and
    /// readable (with its size), it is **not there** (`not there yet`), or it is
    /// there but cannot be read (a directory where a file was expected, a permission
    /// denial, bytes that are not UTF-8) — and then the reason is carried so the
    /// screen says `— the record could not be read: <reason>` instead of calling a
    /// present file absent.
    pub fn instruction_summary(&self) -> Option<(PathBuf, bool, u64, Option<String>)> {
        let paths = self.selected_paths()?;
        let path = paths.instruction().to_path_buf();
        match read_foreign(&path) {
            Ok(Some(bytes)) => Some((path, true, bytes.len() as u64, None)),
            Ok(None) => Some((path, false, 0, None)),
            Err(source) => Some((path, true, 0, Some(source.to_string()))),
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
    ///
    /// The same three cases as the instruction document (Z-3): not there is `none`,
    /// there but unreadable carries the reason, and a readable file is counted.
    pub fn steps_summary(&self) -> Option<(PathBuf, bool, usize, Option<String>)> {
        let paths = self.selected_paths()?;
        let path = paths.steps_file().to_path_buf();
        match read_foreign(&path) {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => {
                    let count = crate::run::parse_steps(&text).len();
                    Some((path, true, count, None))
                }
                Err(source) => Some((
                    path,
                    true,
                    0,
                    Some(format!("the file is not valid UTF-8: {source}")),
                )),
            },
            Ok(None) => Some((path, false, 0, None)),
            Err(source) => Some((path, true, 0, Some(source.to_string()))),
        }
    }

    /// Read every live process and the run records, at most once a second for the
    /// records. The terminal loop calls this on every tick.
    pub fn tick(&mut self) {
        self.supervisor.poll(Instant::now());
        // S-2: a network check that finished on its background thread is applied here, on
        // the UI thread, before the next draw.
        self.poll_probe();
        self.note_stop_proven();
        self.refresh_workspace_note();
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
        self.clear_screen_status();
    }

    /// Show one screen by name.
    pub fn show(&mut self, screen: Screen) {
        if self.screen != screen {
            self.panel_scroll = 0;
            self.clear_screen_status();
        }
        self.screen = screen;
    }

    /// Drop the status sentence made on the screen being left.
    ///
    /// A status sentence describes what just happened *on the screen that made it*.
    /// The scroll sentence (`the panel scrolled — press PageUp or [ to go back`) was
    /// the first one caught doing this (E-1, P-1), and the ninth-round fix cleared
    /// only that family, so the unbound-key sentence, the acknowledgment sentence and
    /// every other screen-local sentence still sat under the next screen and said
    /// something untrue there (C-1). From this round the rule is the mechanism, not a
    /// list: **every** status dies with the screen it was made on, so no sentence can
    /// outlive its screen, and a new sentence cannot be added to a stale family by
    /// mistake. Sentences that must persist are drawn from the state that owns them
    /// (the note areas read records, the acknowledgment line reads the record, the
    /// leave warning owns the keys), never from a status line.
    fn clear_screen_status(&mut self) {
        self.status.clear();
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
            self.show(Screen::AgentConfig);
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
        // An overlay that owns the keys owns a paste too; it must not drop it in
        // silence. Say what owns the screen and how to get to a field, so the paste
        // is not lost without a word (F4). `on_key` says the same for a key.
        if self.show_acknowledgment {
            self.status = if self.ack_review {
                "this is the acknowledged terms, read-only — any key closes it, then paste where a field is open"
                    .to_owned()
            } else {
                "answer this screen first: y to acknowledge, or n for not now".to_owned()
            };
            return;
        }
        if self.show_help {
            self.status =
                "the key list is open — press esc to close it, then paste where a field is open"
                    .to_owned();
            return;
        }
        let (first, rest) = match text.find(['\n', '\r']) {
            Some(at) => (&text[..at], Some(&text[at..])),
            None => (text, None),
        };
        // W32-4: the loss clause is only true when something a person typed was
        // dropped. A terminal's own newline after a copied one-line secret is not
        // paste content, so the sentence names exactly what happened.
        let note = match rest {
            None => "pasted into the field",
            Some(rest) if rest.chars().all(|c| c == '\n' || c == '\r') => {
                "kept the first line; a trailing newline was ignored"
            }
            Some(rest) if rest.trim().is_empty() => {
                "kept the first line; the whitespace after it was ignored"
            }
            Some(_) => "kept the first line; the rest of the paste was not used",
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
        if let Some(prompt) = self.url_prompt.as_mut() {
            prompt.value = first.to_owned();
            self.status = note.to_owned();
            return;
        }
        // The remaining overlays own the keys, so the generic `press n … or k/f/v/A`
        // advice would be false under them. Each names what answers it instead.
        if self.editor_confirm.is_some() {
            self.status =
                "the editor confirmation is open — enter opens it, esc cancels"
                    .to_owned();
            return;
        }
        if self.confirm_remove.is_some() {
            self.status =
                "answer the removal confirmation first: y or enter removes, n, esc or q keeps"
                    .to_owned();
            return;
        }
        if self.confirm_forget.is_some() {
            self.status =
                "answer the delete confirmation first: y or enter deletes, n, esc or q keeps"
                    .to_owned();
            return;
        }
        if self.confirm_quit.is_some() {
            self.status =
                "the leave question is open — q again quits, any other key stays".to_owned();
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
                return;
            }
            // S-3: `ctrl-u` clears the one-line field that is open. It is handled
            // here, before the control guard's blanket return, so the prefilled
            // address and every other field can be replaced with one key instead of
            // backspacing over the old value.
            if key.code == KeyCode::Char('u') {
                if let Some(form) = self.add_agent.as_mut() {
                    form.fields[form.cursor].clear();
                } else if let Some(prompt) = self.secret_prompt.as_mut() {
                    prompt.value.clear();
                } else if let Some(prompt) = self.url_prompt.as_mut() {
                    prompt.value.clear();
                }
            }
            return;
        }
        // A pending "leave and stop the runs?" is cancelled by any key but `q`, and
        // the standing status is replaced: a warning that keeps asking a question
        // already answered reads as a question the product did not hear (D-6).
        if self.confirm_quit.is_some() && key.code != KeyCode::Char('q') {
            // The warning invited any other key to stay, so that key does exactly one
            // thing: it stays. Consuming it keeps a later branch from overwriting the
            // reassurance — an unbound key used to answer `no key is bound to that`
            // and the product never said the run was still going (E-8).
            self.confirm_quit = None;
            self.status =
                "the run continues and this UI stays; q will ask again before leaving".to_owned();
            return;
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
        if self.url_prompt.is_some() {
            self.on_url_key(key);
            return;
        }
        if self.confirm_remove.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirm_remove_agent();
                }
                // `q` is the product's quit key, but under a destructive confirmation
                // it is never the destructive one: it keeps the agent, like `n` and
                // `esc` (E-2). A swallowed key that the footer advertises is worse.
                KeyCode::Char('n')
                | KeyCode::Char('N')
                | KeyCode::Char('q')
                | KeyCode::Esc => {
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
                KeyCode::Char('n')
                | KeyCode::Char('N')
                | KeyCode::Char('q')
                | KeyCode::Esc => {
                    self.confirm_forget = None;
                    self.status = "the leftover secrets were kept".to_owned();
                }
                _ => {}
            }
            return;
        }
        if self.show_acknowledgment {
            // A review is read-only: every key closes it and nothing is recorded.
            if self.ack_review {
                self.close_ack_review();
                return;
            }
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
            KeyCode::Esc => self.show(Screen::Agents),
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
            // W32-1: the configured Forgejo address has its own place. It was
            // settable only by adding an agent, which silently moved it for every
            // existing agent.
            KeyCode::Char('u') if self.screen == Screen::Forgejo => {
                self.request_url_edit();
            }
            KeyCode::Char('A')
                if matches!(self.screen, Screen::Credentials | Screen::Forgejo) =>
            {
                self.request_secret(SecretTarget::ForgejoAdminToken);
            }
            // `R` reads the acknowledged terms again without changing the record, so
            // a second person on a used configuration directory can see what was
            // agreed, when and under which version (C-3).
            KeyCode::Char('R') => self.review_acknowledgment(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char(digit) if ('1'..='6').contains(&digit) => {
                let at = digit as usize - '1' as usize;
                // `show` resets the panel's scroll offset: a screen the user never
                // scrolled must not open at another screen's offset (A-1).
                self.show(Screen::ALL[at]);
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

/// The template text for a file that must exist before it can be edited, so the editor
/// never opens on a path that is not there. A skill gets a scratch `SKILL.md`, because
/// a skill is one directory holding that manifest, never a directory handed to an
/// editor. The text is written only at the hand-off (see `take_action`).
fn editable_template(target: EditTarget, agent: &str) -> String {
    match target {
        // The guidance is a single marked placeholder, not prose in the document: the
        // run drops this text from the document's first content line before it sends
        // the document, so a person who types below the guidance — or into the line the
        // editor opens on — never sends the guidance itself (T-1).
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
    }
}

/// The first absolute path a stored sentence names that is not the live file.
///
/// Only the shape a path is written in matters here: a token that begins with `/`,
/// with the sentence's own trailing punctuation removed. Anything else is prose.
fn foreign_path(text: &str, live: &str) -> Option<String> {
    text.split_whitespace()
        .map(|token| token.trim_end_matches(['.', ',', ';', ':', ')']))
        .find(|token| token.starts_with('/') && *token != live)
        .map(str::to_owned)
}

/// Read a file the product did not write, keeping "not there" apart from "there but
/// unreadable" (Z-3, W38-1's class).
///
/// `Ok(None)` means only `ENOENT`: there is no file. A directory where a file was
/// expected, a permission denial, a broken symlink and any other failure are `Err`,
/// so a screen can say the file could not be read instead of calling it absent. This
/// is the single-file sibling of [`crate::transcript::read_checked`], which does the
/// same for a run's transcript.
fn read_foreign(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(source),
    }
}

/// Write a hand-off's template, creating the directories a skill needs.
fn write_template(path: &std::path::Path, template: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // F-1: the directory this hand-off makes (a new skill's directory) is the
        // product's, and carries the product's custody.
        crate::mode::set_mode(parent, crate::mode::PRIVATE_DIR_MODE)?;
    }
    crate::mode::write_private(path, template)
}

/// The status when the hand-off created the file it opened and the editor left the
/// template untouched.
///
/// `not changed` would be false: the Instruction screen now counts the skill, or
/// shows the steps file, that this hand-off just made, and the next run installs
/// whatever is under `skills/` (H-1, H-2). The sentence names the creation and what
/// the screen beside it will now show.
fn created_status(action: &PendingAction, program: &str) -> String {
    let file = action
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("the file");
    match action.target {
        Some(EditTarget::Skills) => format!(
            "{} was created as a template ({file}) and left unchanged ({program}); a run installs every skill under `skills/`, so write it or remove the directory",
            action.label
        ),
        Some(EditTarget::Steps) => format!(
            "{} did not exist; the template ({file}) was created and left unchanged ({program}) — it declares 0 shell steps, so the screen now shows the file",
            action.label
        ),
        _ => format!(
            "{} did not exist; the template ({file}) was created and left unchanged ({program})",
            action.label
        ),
    }
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

    #[test]
    fn a_file_size_reads_in_the_products_own_words() {
        // L-3: the log and the transcript grow with every run; their sizes are shown in
        // a unit a person reads, not as a raw byte count.
        assert_eq!(filesize_text(0), "0 B");
        assert_eq!(filesize_text(1023), "1023 B");
        // F-2: the number is bytes/1024, so the label is the binary unit. `4.0 KB`
        // over this value named a definition the number did not use.
        assert_eq!(filesize_text(4096), "4.0 KiB");
        assert_eq!(filesize_text(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(filesize_text(3 * 1024 * 1024 * 1024), "3.0 GiB");
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
instruction = "agents/one/instruction.md"
concurrency = 1

[[agents]]
name = "two"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/two/instruction.md"
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
    fn a_configured_instruction_path_is_the_one_every_screen_and_the_run_use() {
        // P-3: the field used to be echoed on the Agent screen and silently ignored.
        // A path with a space, outside the derived default, proves that the path the
        // Instruction screen reads, the path the run plan carries and the path the
        // start checks are one and the same.
        let root = scratch_root("app-instruction-field");
        let config = r#"
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
instruction = "my instructions/agent one.md"
concurrency = 1
"#;
        std::fs::write(root.join("config.toml"), config).expect("the configuration is written");
        let mut app = App::with_runner(
            Layout::rooted_at(root.clone()),
            quiet_runner("app-instruction-field"),
        );
        app.on_key(key(KeyCode::Char('y')));
        let configured = root.join("my instructions/agent one.md");
        std::fs::create_dir_all(configured.parent().expect("a parent")).expect("the directory");
        std::fs::write(&configured, "do something\n").expect("the instruction is written");
        let derived = app
            .layout
            .agent("one")
            .expect("the derived paths")
            .instruction()
            .to_path_buf();
        assert_ne!(derived, configured, "the probe must not be the derived default");
        let summary = app
            .instruction_summary()
            .expect("the selected agent has an instruction");
        assert_eq!(
            summary.0, configured,
            "the Instruction screen names the configured file"
        );
        assert!(summary.1, "the configured file is there");
        assert!(
            !derived.exists(),
            "the derived path is not invented beside the configured one"
        );
        give_key(&app, "one");
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("started `one`"), "{}", app.status);
        app.shutdown();
    }

    #[test]
    fn no_status_sentence_outlives_the_screen_it_was_made_on() {
        // E-1, P-1 then C-1: the scroll offset was reset on a screen change but the
        // sentence about it was not; the ninth round cleared only that family, so the
        // unbound-key sentence and the acknowledgment sentence still sat under the
        // next screen. From this round **every** status dies with the screen it was
        // made on, so a status added later cannot join a stale family by mistake.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.show(Screen::Agents);
        app.scroll_panel_edge(true);
        assert!(app.status.starts_with("the panel"), "{}", app.status);
        app.show(Screen::AgentConfig);
        assert!(
            app.status.is_empty(),
            "a scroll sentence outlived its screen: {}",
            app.status
        );

        // The unbound-key sentence is not on the hub (C-1's second example).
        app.show(Screen::Agents);
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.status.contains("no key is bound"), "{}", app.status);
        app.show(Screen::Instructions);
        assert!(
            app.status.is_empty(),
            "the unbound-key sentence outlived its screen: {}",
            app.status
        );

        // A refusal, a run's sentence and a "kept" receipt are the same class: they
        // are made on a screen, and a screen later they are not the truth any more.
        // Nothing persists through the status line — what persists is state drawn
        // from the record that owns it (the note areas), not a sentence.
        let made = "not started: no model key for `one`";
        app.status = made.to_owned();
        app.shift_screen(1);
        assert!(
            app.status.is_empty(),
            "a status outlived its screen: {}",
            app.status
        );

        // The same screen keeps its own sentence: `show` of the screen already on
        // show is not a screen change.
        app.status = made.to_owned();
        app.show(app.screen);
        assert_eq!(app.status, made);
    }

    #[test]
    fn q_keeps_at_a_destructive_confirmation_instead_of_being_swallowed() {
        // E-2: `q` was the product's quit key and did nothing at all in the two
        // destructive confirmations, while the footer advertised `q quit`. It now
        // keeps, exactly like `n` and `esc`, so the advertised key is never silent
        // and never the destructive one by accident.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.confirm_remove = Some("one".to_owned());
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.confirm_remove.is_none(), "the confirmation stayed");
        assert!(app.status.contains("was kept"), "{}", app.status);
        assert_eq!(app.agents().len(), 2, "the agent was removed by `q`");

        app.confirm_forget = Some(vec!["ghost".to_owned()]);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.confirm_forget.is_none(), "the leftover confirmation stayed");
        assert!(
            app.status.contains("leftover secrets were kept"),
            "{}",
            app.status
        );
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
        // N-1: the program is named before it opens, so the hand-off waits on the
        // confirmation; `enter` is the key that opens it.
        assert!(app.editor_confirm.is_some(), "the program is named first");
        app.on_key(key(KeyCode::Enter));
        // Nothing is written until the hand-off begins; a cancelled confirmation
        // leaves the directory as it was (H-1, H-2).
        assert!(!paths.instruction().exists(), "no file before the hand-off");
        let action = app.take_action().expect("an edit was asked for");
        assert_eq!(action.kind, ActionKind::Edit);
        assert_eq!(action.path, paths.instruction());
        assert!(action.created, "the hand-off created the file");
        assert!(paths.instruction().is_file(), "the file is created");
        assert!(app.take_action().is_none(), "the action is taken once");
    }

    #[test]
    fn editing_the_steps_creates_the_file_and_the_skills_directory() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        let paths = app.selected_paths().expect("paths");
        app.on_key(key(KeyCode::Char('t')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("a steps edit");
        assert_eq!(action.path, paths.steps_file());
        assert!(action.created, "the hand-off created the steps file");
        assert!(paths.steps_file().is_file());
        // N-10: the steps template must not doubt the product by saying model calls
        // do not exist yet; they do, and the template describes when the steps run.
        let template = std::fs::read_to_string(paths.steps_file()).expect("the steps read");
        assert!(!template.contains("Until model calls exist"), "{template}");
        assert!(template.contains("before the model is"), "{template}");
        app.on_key(key(KeyCode::Char('K')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("a skills edit");
        // `K` opens a scratch `SKILL.md`, never a directory.
        assert_eq!(action.path, paths.skills_dir().join("new-skill").join("SKILL.md"));
        assert!(action.created, "the hand-off created the skill");
        assert!(action.path.is_file());
        assert!(paths.skills_dir().is_dir());
    }

    #[test]
    fn an_editor_that_fails_still_says_the_template_was_created() {
        // The H-1/H-2 class has no hole when the editor itself fails: the file the
        // hand-off made is named in the failure sentence too.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        app.on_key(key(KeyCode::Char('K')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("an edit");
        let failure = std::process::Command::new("false")
            .status()
            .expect("`false` runs");
        app.note_action(&action, Ok(failure));
        assert!(
            app.status.contains("template was created"),
            "{}",
            app.status
        );
    }

    #[test]
    fn an_interrupted_editor_return_names_recovery_and_says_the_keys_are_back() {
        // R-4: the return from an interrupted editor read `code None` (a Rust
        // `Option` on a screen), did not name the emergency-save file the product had
        // just detected, and did not carry the return clause every other hand-off
        // outcome has.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("an edit");
        let backup = action.path.with_file_name(".instruction.md.swp");
        std::fs::write(&backup, "unsaved words").expect("the emergency save is written");
        let interrupted = std::process::Command::new("sh")
            .arg("-c")
            .arg("kill -9 $$")
            .status()
            .expect("the shell runs");
        assert!(
            interrupted.code().is_none(),
            "the test really interrupted the editor"
        );
        app.note_action(&action, Ok(interrupted));
        assert!(app.status.contains("was interrupted"), "{}", app.status);
        assert!(
            !app.status.contains("code None"),
            "a Rust `Option` reached the screen: {}",
            app.status
        );
        assert!(
            app.status.contains(".instruction.md.swp"),
            "the recovery file is not named: {}",
            app.status
        );
        assert!(
            app.status.contains("the keys are the product's again"),
            "the return is silent: {}",
            app.status
        );
    }

    #[test]
    fn a_prefilled_field_can_be_cleared_with_one_key() {
        // S-3: the prefilled address could only be corrected by backspacing over it,
        // and a person who typed appended to the old value instead. `ctrl-u` clears
        // the field on every one-line prompt.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.request_url_edit();
        assert!(
            !app.url_prompt
                .as_ref()
                .expect("the address prompt")
                .value
                .is_empty(),
            "the address prompt is prefilled"
        );
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            app.url_prompt.as_ref().expect("the address prompt").value,
            "",
            "ctrl-u did not clear the address"
        );
        app.url_prompt = None;
        app.request_secret(SecretTarget::ModelKey);
        app.on_key(key(KeyCode::Char('x')));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            app.secret_prompt.as_ref().expect("the secret prompt").value,
            "",
            "ctrl-u did not clear the secret prompt"
        );
        app.secret_prompt = None;
        app.request_new_agent();
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            app.add_agent.as_ref().expect("the form").fields[0],
            "",
            "ctrl-u did not clear the form field"
        );
    }

    #[test]
    fn backing_out_of_an_editor_confirmation_creates_no_file() {
        // H-1/H-2's sibling: the file used to be made when the key was pressed, so a
        // person who pressed `esc` on `no $EDITOR … press enter` was left with a
        // counted skill or a steps file they never opened.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = false;
        let paths = app.selected_paths().expect("paths");
        for (keycode, path) in [
            (KeyCode::Char('K'), paths.skills_dir().join("new-skill").join("SKILL.md")),
            (KeyCode::Char('t'), paths.steps_file().to_path_buf()),
            (KeyCode::Char('e'), paths.instruction().to_path_buf()),
        ] {
            app.on_key(key(keycode));
            assert!(app.editor_confirm.is_some(), "the confirmation is up");
            app.on_key(key(KeyCode::Esc));
            assert!(app.take_action().is_none(), "nothing is handed off");
            assert!(!path.exists(), "{} was not created", path.display());
        }
    }

    #[test]
    fn an_untouched_new_file_is_reported_as_created_not_as_not_changed() {
        // H-1: `K` created a counted skill and said `was not changed`; H-2: `t`
        // created a steps file and said `was not changed`. One guard per hand-off key:
        // the status must name the creation the screen beside it can see.
        // W-4: the sentence names the file it made, not only the thing.
        let cases = [
            (KeyCode::Char('K'), "created as a template (SKILL.md)"),
            (KeyCode::Char('t'), "the template (steps.txt) was created"),
            (KeyCode::Char('e'), "the template (instruction.md) was created"),
        ];
        for (keycode, expected) in cases {
            let mut app = app_with(GOOD);
            app.show_acknowledgment = false;
            app.editor_configured = true;
            app.on_key(key(keycode));
            app.on_key(key(KeyCode::Enter));
            let action = app.take_action().expect("an edit");
            assert!(action.created, "the hand-off created the file");
            // The editor ran and saved nothing: the fingerprint is unchanged.
            app.note_action(&action, Ok(success()));
            assert!(app.status.contains(expected), "{} for {keycode:?}", app.status);
            assert!(
                !app.status.contains("not changed"),
                "a created file was called unchanged: {}",
                app.status
            );
        }
    }

    #[test]
    fn opening_a_log_with_no_run_says_so_rather_than_opening_something() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('o')));
        // W32-3: the sentence names what **this machine** knows, not what the agent
        // did on another one.
        assert!(
            app.status.contains("no run record on this machine yet"),
            "{}",
            app.status
        );
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
        // F-E: never claim an edit that was not written. The file must already exist:
        // a hand-off that *creates* one reports the creation instead (H-1, H-2).
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        let paths = app.selected_paths().expect("paths");
        std::fs::create_dir_all(paths.instruction().parent().expect("a parent"))
            .expect("the agent directory exists");
        std::fs::write(paths.instruction(), "an instruction already written\n")
            .expect("the instruction exists");
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("an edit was asked for");
        assert!(!action.created, "the file was already there");
        app.note_action(&action, Ok(success()));
        assert!(app.status.contains("was not changed"), "{}", app.status);
    }

    #[test]
    fn an_edit_that_changed_the_file_is_reported_as_edited() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.editor_configured = true;
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Enter));
        let action = app.take_action().expect("an edit was asked for");
        std::fs::write(&action.path, "a changed instruction\n").expect("the file is changed");
        app.note_action(&action, Ok(success()));
        assert!(app.status.contains("was edited"), "{}", app.status);
    }

    #[test]
    fn a_hand_off_says_the_keys_are_the_products_again() {
        // T-2: an editor that leaves at once (an untouched template) hands the keys
        // back with no sentence; a habitual editor key then acts on the product. The
        // hand-over is now visible in the after direction too.
        for (keycode, expected) in [
            (KeyCode::Char('t'), "created"),
            (KeyCode::Char('e'), "created"),
        ] {
            let mut app = app_with(GOOD);
            app.show_acknowledgment = false;
            app.editor_configured = true;
            app.on_key(key(keycode));
            app.on_key(key(KeyCode::Enter));
            let action = app.take_action().expect("an edit");
            app.note_action(&action, Ok(success()));
            assert!(app.status.contains(expected), "{}", app.status);
            assert!(
                app.status.contains("the keys are the product's again"),
                "{}",
                app.status
            );
        }
        // A view returns the keys too.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let run_id = "run-1-0001";
        let dir = app.layout.state_root().join("one").join(run_id);
        std::fs::create_dir_all(&dir).expect("the run directory");
        std::fs::write(dir.join("run.log"), "a log\n").expect("a log");
        app.refresh_recent();
        app.on_key(key(KeyCode::Char('o')));
        if let Some(action) = app.take_action() {
            app.note_action(&action, Ok(success()));
            assert!(
                app.status.contains("the keys are the product's again"),
                "{}",
                app.status
            );
        }
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
        // A-2: the refusal names the product's own key for the fix, not a file and a
        // TOML section.
        assert!(!app.status.contains("[model_keys]"), "{}", app.status);
        assert!(
            app.status.contains("k on the Credentials screen"),
            "{}",
            app.status
        );
        assert_eq!(app.supervisor.running_count(), 0);
    }

    #[test]
    fn a_missing_forgejo_token_refusal_names_the_products_own_keys() {
        // A-2's class: the sibling refusal (`i`, with no administrator token and no
        // agent token) names `v`/`A`, not `[forgejo_agent_tokens]`. No network is
        // touched: the token is read before any request.
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Char('i')));
        app.wait_for_probe();
        assert!(app.status.contains("agent token"), "{}", app.status);
        assert!(app.status.contains("absent"), "{}", app.status);
        assert!(
            !app.status.contains("[forgejo_agent_tokens]"),
            "{}",
            app.status
        );
        assert!(
            !app.status.contains("credentials file"),
            "the refusal sends a person to a file: {}",
            app.status
        );
        assert!(
            app.status.contains("v on the Forgejo screen"),
            "{}",
            app.status
        );
    }

    #[test]
    fn the_role_check_refusal_names_the_products_own_keys() {
        // T-4: the role-token refusal drew the credentials file and the TOML key
        // `forgejo_token`; its sibling (the missing model key) names the product's own
        // keys. `f` on the Credentials screen is what writes `forgejo_token`.
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Char('c')));
        assert!(app.status.contains("role token absent"), "{}", app.status);
        assert!(
            app.status.contains("f on the Credentials screen"),
            "{}",
            app.status
        );
        assert!(!app.status.contains("forgejo_token"), "{}", app.status);
        assert!(!app.status.contains("credentials.toml"), "{}", app.status);
    }

    /// Whether a refusal names something a person can act on: a product key, an exact
    /// TOML section the file itself uses, or a concrete path to a file.
    fn names_a_key_section_or_path(text: &str) -> bool {
        if text.contains("press ") {
            return true;
        }
        if text.contains('[') && text.contains(']') {
            return true;
        }
        text.contains('/')
            && (text.contains(".toml")
                || text.contains(".md")
                || text.contains(".jsonl")
                || text.contains(".log"))
    }

    /// The class a refusal draws, when it draws one: the word in the first `(…)` after
    /// the word `failed`. `None` means the sentence carries no class at all.
    fn class_in_refusal(text: &str) -> Option<String> {
        let at = text.find("failed (")? + "failed (".len();
        let rest = &text[at..];
        let end = rest.find(')')?;
        Some(rest[..end].to_owned())
    }

    #[test]
    fn every_secret_refusal_a_person_acts_on_names_the_key_section_or_path_to_act_on() {
        // V-1 conformance guard, extended by F-3.
        //
        // How it enumerates: it drives each refusal path through the built `App`
        // (the same key a person presses), reads the sentence the product draws, and
        // checks every one. It then reads the source embedded at compile time
        // (`include_str!`, so the built binary carries the list) for every refusal
        // class literal that concerns a secret and fails if one was not among the
        // driven paths — so a new `missing_*`/`*_token` refusal cannot be added
        // without a driven row and a sentence that names how to act.
        //
        // F-3: naming the key is not enough; the **class** must name the same thing
        // every time. A missing role token drawn under `credentials` — the class for a
        // credentials file that cannot be read — sends a reader to the file. Each
        // driven row now carries the class it must draw, and that class is compared
        // against the sentence itself.
        struct Refusal {
            path: &'static str,
            /// The class this refusal must draw, when it draws one at all.
            class: Option<&'static str>,
            status: String,
        }
        let mut produced: Vec<Refusal> = Vec::new();

        // `c`: no role token is recorded.
        {
            let mut app = app_with(GOOD);
            app.on_key(key(KeyCode::Char('y')));
            app.on_key(key(KeyCode::Char('c')));
            produced.push(Refusal {
                path: "c role token",
                class: Some("no_role_token"),
                status: app.status.clone(),
            });
        }
        // `C`: no administrator token is recorded.
        {
            let mut app = app_with(GOOD);
            app.on_key(key(KeyCode::Char('y')));
            app.on_key(key(KeyCode::Char('C')));
            produced.push(Refusal {
                path: "C administrator token",
                class: Some("missing_admin_token"),
                status: app.status.clone(),
            });
        }
        // `i`: no agent token and no administrator token.
        {
            let mut app = app_with(GOOD);
            app.on_key(key(KeyCode::Char('y')));
            app.on_key(key(KeyCode::Char('i')));
            app.wait_for_probe();
            produced.push(Refusal {
                path: "i agent token",
                class: Some("no_agent_token"),
                status: app.status.clone(),
            });
        }
        // `s`: no model key, with an instruction so the refusal is reached.
        {
            let mut app = app_with(GOOD);
            app.on_key(key(KeyCode::Char('y')));
            let paths = app.layout.ensure_agent("one").expect("the agent is prepared");
            std::fs::write(paths.instruction(), "do something\n").expect("the instruction");
            app.on_key(key(KeyCode::Char('s')));
            produced.push(Refusal {
                path: "s model key",
                // This refusal leads with the fix rather than a `failed (class)`
                // clause; the key it names is what the guard checks for it.
                class: None,
                status: app.status.clone(),
            });
        }
        // `p`: the endpoint test finds no model key.
        {
            let mut app = app_with(GOOD);
            app.on_key(key(KeyCode::Char('y')));
            app.on_key(key(KeyCode::Char('p')));
            app.wait_for_probe();
            produced.push(Refusal {
                path: "p endpoint test",
                class: Some("missing_key"),
                status: app.status.clone(),
            });
        }

        for row in &produced {
            assert!(
                names_a_key_section_or_path(&row.status),
                "V-1: the refusal from `{}` names no key, section or path a person \
                 can act on: {}",
                row.path,
                row.status
            );
            // F-3: the class must name the thing that is missing, not a neighbouring
            // class. `credentials` here would read as a broken credentials file.
            let drawn = class_in_refusal(&row.status);
            if let Some(expected) = row.class {
                assert_eq!(
                    drawn.as_deref(),
                    Some(expected),
                    "F-3: the refusal from `{}` draws the class {:?}, not `{expected}`: {}",
                    row.path,
                    drawn,
                    row.status
                );
            }
            assert!(
                drawn.as_deref() != Some("credentials"),
                "F-3: a missing key is reported under the class `credentials`, which names \
                 a credentials file that cannot be read: {}",
                row.status
            );
        }

        let source = include_str!("app.rs");
        for (at, _) in source.match_indices("class: \"") {
            let rest = &source[at + "class: \"".len()..];
            let Some(end) = rest.find('"') else {
                continue;
            };
            let class = &rest[..end];
            if !(class.contains("token") || class.starts_with("missing_")) {
                continue;
            }
            assert!(
                produced
                    .iter()
                    .any(|row| row.status.contains(&format!("({class})"))),
                "V-1: the secret-refusal class `{class}` is declared in the source but no \
                 driven path above produces it; add its path and make it name its key"
            );
        }
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
            machine: None,
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
    fn an_endpoint_test_is_written_down_and_read_back() {
        // LIB-2: the stamp was session-only, so after a restart the Agent screen drew
        // no `endpoint test` field at all. The test is a file beside the
        // acknowledgment record now, so the stamp survives and ages.
        let root = scratch_root("endpoint-record");
        let path = root.join("endpoint-tests.toml");
        assert!(read_endpoint_tests(&path).is_empty(), "no file is no test");
        let one = EndpointTest {
            agent: "one".to_owned(),
            endpoint: "https://m.example.org".to_owned(),
            model: "m".to_owned(),
            result: Ok("the endpoint answered: ok".to_owned()),
            tested_at_ms: 1_700_000_000_000,
            machine: None,
        };
        write_endpoint_test(&path, &one).expect("the test is written");
        let two = EndpointTest {
            agent: "two".to_owned(),
            endpoint: "https://m.example.org".to_owned(),
            model: "m".to_owned(),
            result: Err(EndpointFailure {
                class: "transport".to_owned(),
                message: "the endpoint refused the connection".to_owned(),
            }),
            tested_at_ms: 1_700_000_001_000,
            machine: None,
        };
        write_endpoint_test(&path, &two).expect("the second test is written");
        let mut back = read_endpoint_tests(&path);
        back.sort_by(|left, right| left.agent.cmp(&right.agent));
        assert_eq!(back, vec![one.clone(), two.clone()]);
        // Re-testing one agent replaces only its own record.
        let retested = EndpointTest {
            tested_at_ms: 1_700_000_002_000,
            result: Ok("a fresh answer".to_owned()),
            ..one.clone()
        };
        write_endpoint_test(&path, &retested).expect("the retest is written");
        let mut back = read_endpoint_tests(&path);
        back.sort_by(|left, right| left.agent.cmp(&right.agent));
        assert_eq!(back, vec![retested, two]);
        // A file the product writes is held to its one custody.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("the record").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the endpoint record is not private");
    }

    #[test]
    fn a_restart_draws_the_persisted_endpoint_test_instead_of_nothing() {
        // LIB-2 at the screen: a test written in one run is the field the Agent screen
        // draws in the next, and an agent with no test says `not tested yet`.
        let root = scratch_root("endpoint-restart");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration");
        write_endpoint_test(
            &root.join("endpoint-tests.toml"),
            &EndpointTest {
                agent: "one".to_owned(),
                endpoint: "https://m.example.org".to_owned(),
                model: "m".to_owned(),
                result: Ok("the endpoint answered: ok".to_owned()),
                tested_at_ms: 1_700_000_000_000,
                machine: None,
            },
        )
        .expect("the test is written");
        let app = App::with_runner(Layout::rooted_at(root), quiet_runner("endpoint-restart"));
        let test = app.endpoint_test_for("one").expect("the record is read back");
        assert!(test.result.is_ok());
        assert!(app.endpoint_test_for("two").is_none());
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
            read_error: None,
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
            read_error: None,
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
            read_error: None,
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
        // O1: the hub seeds `stopped` while the Runs list used to keep reading the
        // record it held before the reconciliation, so the two screens disagreed for a
        // moment about whether the abandoned run was still running. Reading the records
        // back after the reconciliation closes that window.
        let newest = app
            .recent
            .iter()
            .find(|run| run.agent == "one")
            .expect("the run is listed");
        assert_eq!(
            newest.outcome,
            Some(crate::run::RunOutcome::Stopped),
            "the Runs list still says the abandoned run is running"
        );
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
instruction = "agents/two/instruction.md"
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
    fn a_paste_with_no_field_open_is_never_read_as_keys_and_says_where_it_can_land() {
        // F4: a token pasted at a screen with no field open opened the add-agent form,
        // because the paste was read as keystrokes. A bracketed paste is one event and
        // is never replayed as keys; with no field open it is dropped with a sentence
        // that names where it can land.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.show(Screen::Forgejo);
        // The token begins with `g` (a clone binding) and contains `t` (the editor
        // binding) and `a` (the form binding); none of them may fire.
        app.on_paste("ga-fixture-token-abcd");
        assert!(app.add_agent.is_none(), "the paste opened the add-agent form");
        assert!(app.secret_prompt.is_none(), "the paste opened a secret prompt");
        assert!(app.editor_confirm.is_none(), "the paste asked to open an editor");
        assert_eq!(
            app.status,
            "there is no one-line field here to take a paste; press n for the form, or k/f/v/A for a secret"
        );
        // A field that is open takes the paste, one line only.
        app.show(Screen::Credentials);
        app.on_key(key(KeyCode::Char('k')));
        app.on_paste("secret");
        assert_eq!(app.secret_prompt.as_ref().expect("a prompt").value, "secret");
    }

    #[test]
    fn a_paste_under_an_overlay_is_not_dropped_in_silence() {
        // F4's sibling path: the acknowledgment and the key list own the keys, and they
        // own a paste too — but a paste is still never dropped without a word.
        let mut app = app_with(GOOD);
        assert!(app.show_acknowledgment, "the first run shows the modal");
        app.on_paste("token");
        assert!(
            app.status.contains("answer this screen first"),
            "{}",
            app.status
        );
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.show_help, "the key list is open");
        app.on_paste("token");
        assert!(app.status.contains("key list is open"), "{}", app.status);
    }

    #[test]
    fn every_overlay_answers_a_paste_with_what_owns_the_screen() {
        // F4's sweep: the generic `press n … or k/f/v/A` advice is false under a
        // confirmation that owns the keys, so each overlay names its own answer.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.confirm_remove = Some("one".to_owned());
        app.on_paste("token");
        assert!(app.status.contains("removal confirmation"), "{}", app.status);
        app.confirm_remove = None;
        app.confirm_forget = Some(vec!["ghost".to_owned()]);
        app.on_paste("token");
        assert!(app.status.contains("delete confirmation"), "{}", app.status);
        app.confirm_forget = None;
        app.confirm_quit = Some(1);
        app.on_paste("token");
        assert!(app.status.contains("leave question"), "{}", app.status);
        app.confirm_quit = None;
        app.editor_configured = false;
        app.show(Screen::Instructions);
        app.on_key(key(KeyCode::Char('t')));
        assert!(app.editor_confirm.is_some(), "the editor confirmation is up");
        app.on_paste("token");
        assert!(app.status.contains("editor confirmation"), "{}", app.status);
    }

    #[test]
    fn the_identity_status_does_not_repeat_the_report_the_panel_carries() {
        // W-3: the report is drawn once, on the Forgejo panel. The status is a receipt;
        // a failure keeps its actionable message.
        use crate::identity::{AgentIdentity, IdentityMode};
        let settled = Ok(AgentIdentity {
            agent: "one".to_owned(),
            login: "gewu-one".to_owned(),
            email: "gewu-one@agents.invalid".to_owned(),
            mode: IdentityMode::Managed,
            created: true,
            created_at: Some(1_700_000_000),
        });
        let receipt = identity_receipt("one", &settled);
        assert!(!receipt.contains("gewu-one@agents.invalid"), "{receipt}");
        assert!(!receipt.contains("(created"), "{receipt}");
        let refused = Err(ForgejoFailure {
            class: "no_agent_token".to_owned(),
            message: "press v on the Forgejo screen to supply it".to_owned(),
        });
        let receipt = identity_receipt("one", &refused);
        assert!(
            receipt.contains("press v on the Forgejo screen"),
            "{receipt}"
        );
    }

    #[test]
    fn a_moved_configurations_record_names_the_dead_path_and_the_live_file() {
        // W-1: a copied configuration leaves a record whose reason names the old path;
        // the drawn sentence says the path is the run's own and names the file now.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.selected = 0;
        let live = app
            .selected_paths()
            .expect("paths")
            .instruction()
            .display()
            .to_string();
        let stored = "the instruction document /old/place/agents/one/instruction.md is still the placeholder; write what the agent should do".to_string();
        let drawn = app.reason_with_history("one", &stored);
        assert!(drawn.contains("/old/place/agents/one/instruction.md"), "{drawn}");
        assert!(drawn.contains("from when that run ran"), "{drawn}");
        assert!(drawn.contains(&live), "{drawn}");
        // A reason that names the file in use gains no clause.
        let current = format!("the instruction document {live} is still the placeholder");
        assert_eq!(app.reason_with_history("one", &current), current);
    }

    #[test]
    fn the_age_ladder_has_a_rung_for_every_unit_it_names() {
        // W-6: the first step after `just now` was a full minute. The ladder is one
        // ladder for every age string, and seconds sit between the two.
        let now = crate::time::now_unix_ms();
        let ago = |seconds: u64| tested_age(now.saturating_sub(seconds.saturating_mul(1000)));
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(30), "30 seconds ago");
        assert_eq!(ago(5 * 60), "5 minutes ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(2 * 86_400), "2 days ago");
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
    fn a_paste_that_only_trailed_a_newline_is_not_reported_as_lost_content() {
        // W32-4: a token copied with a trailing newline drew `the rest of the paste was
        // not used`, though nothing a person typed was dropped. The sentence now names
        // exactly what happened.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.show(Screen::Credentials);
        app.on_key(key(KeyCode::Char('k')));
        app.on_paste("a-real-looking-token\n");
        assert_eq!(
            app.status,
            "kept the first line; a trailing newline was ignored"
        );
        assert_eq!(
            app.secret_prompt.as_ref().expect("the prompt").value,
            "a-real-looking-token"
        );
        // Whitespace that is not a newline is named as whitespace, not as lost text.
        app.on_paste("another-token\n   ");
        assert_eq!(
            app.status,
            "kept the first line; the whitespace after it was ignored"
        );
        // Real lost content keeps the loss clause: a person did type it.
        app.on_paste("first\nsecond");
        assert_eq!(
            app.status,
            "kept the first line; the rest of the paste was not used"
        );
    }

    #[test]
    fn a_refused_secret_write_replaces_the_status_it_started_from() {
        // F-2: after a refused write the footer still read `pasted into the field`.
        // The refusal replaces the optimistic sentence; the prompt holds the same
        // answer, so the footer and the box agree.
        let root = scratch_root("refused-secret-status");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration");
        let mut app =
            App::with_runner(Layout::rooted_at(root), quiet_runner("refused-secret-status"));
        app.show_acknowledgment = false;
        app.show(Screen::Credentials);
        // Widen the file so the write itself is refused for its mode.
        let path = app.layout.credentials_file();
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, "").expect("the file");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("the widened mode");
        }
        app.on_key(key(KeyCode::Char('k')));
        app.on_paste("a-throwaway-key");
        assert_eq!(app.status, "pasted into the field");
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.status.starts_with("not written:"),
            "the optimistic paste sentence survived the refusal: {}",
            app.status
        );
        let error = app
            .secret_prompt
            .as_ref()
            .expect("the prompt stays open")
            .error
            .clone()
            .expect("the refusal is in the prompt");
        assert!(app.status.contains(&error), "the box and the footer disagree");
        // An empty secret is the second refusal path, and it replaces the status too.
        app.secret_prompt.as_mut().expect("open").value.clear();
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.status.contains("empty secret is refused"),
            "{}",
            app.status
        );
    }

    #[test]
    fn the_forgejo_address_has_its_own_key_and_changes_only_the_address() {
        // W32-1: the address was settable only by adding an agent, and that form moved
        // the global address for every agent. `u` edits it on its own.
        let root = scratch_root("forgejo-url-key");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration");
        let mut app = App::with_runner(Layout::rooted_at(root), quiet_runner("forgejo-url"));
        app.show_acknowledgment = false;
        app.show(Screen::Forgejo);
        app.on_key(key(KeyCode::Char('u')));
        let prompt = app.url_prompt.as_ref().expect("the address prompt is open");
        assert_eq!(prompt.value, "https://forgejo.example.org");
        // The prompt is prefilled, so a person can correct one character.
        app.url_prompt.as_mut().expect("open").value.clear();
        for character in "http://127.0.0.1:39634".chars() {
            app.on_key(key(KeyCode::Char(character)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.url_prompt.is_none(), "the prompt closed on success");
        assert!(
            app.status.contains("http://127.0.0.1:39634"),
            "{}",
            app.status
        );
        // Only the address changed: both agents and their endpoints survive.
        let config = app.config.as_ref().expect("the configuration is re-read");
        assert_eq!(config.forgejo.url, "http://127.0.0.1:39634");
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].endpoint, "https://m.example.org");
    }

    #[test]
    fn the_form_says_when_it_moved_the_address_for_existing_agents() {
        // W32-1's sibling: the add-agent form still writes the global address, but it
        // no longer does so silently.
        let root = scratch_root("form-moved-url");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration");
        let mut app = App::with_runner(Layout::rooted_at(root), quiet_runner("form-moved-url"));
        app.show_acknowledgment = false;
        app.on_key(key(KeyCode::Char('n')));
        let fields = [
            "third",
            "https://m.example.org",
            "m",
            "gw-throwaway-key",
            "http://127.0.0.1:39999",
            "",
        ];
        for (at, value) in fields.iter().enumerate() {
            if at > 0 {
                app.on_key(key(KeyCode::Tab));
            }
            for character in value.chars() {
                app.on_key(key(KeyCode::Char(character)));
            }
        }
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.status.contains("moved the Forgejo address"),
            "the form moved every agent's address silently: {}",
            app.status
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
            target: Some(EditTarget::Steps),
            created: false,
            template: None,
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

    #[test]
    fn the_role_check_stamp_survives_a_restart() {
        // W37-1: the v0.1.18 note promises a persisted stamp. The check writes
        // `role-checks.toml`; the next start reads it back even though its own view
        // is gone.
        let root = scratch_root("role-stamp");
        std::fs::write(root.join("config.toml"), GOOD).expect("the configuration is written");
        let mut app =
            App::with_runner(Layout::rooted_at(root.clone()), quiet_runner("role-stamp-1"));
        app.show_acknowledgment = false;
        app.apply_role_check(
            "https://forgejo.example.org".to_owned(),
            Err(ForgejoFailure {
                class: "transport".to_owned(),
                message: "the connection dropped".to_owned(),
            }),
        );
        assert!(app.role_check_stamp().is_some());
        let record = root.join("role-checks.toml");
        assert!(record.is_file(), "the stamp was not written down");
        let text = std::fs::read_to_string(&record).expect("the record is read");
        assert!(text.contains("checked_at_ms"), "{text}");

        let reopened =
            App::with_runner(Layout::rooted_at(root), quiet_runner("role-stamp-2"));
        assert!(
            reopened.role_check.is_none(),
            "a new session has no role view of its own"
        );
        let stamp = reopened
            .role_check_stamp()
            .expect("the stamp survives the restart");
        assert!(stamp.contains("checked"), "{stamp}");
    }

    #[test]
    fn the_role_token_is_reported_for_the_connection_not_an_agent() {
        // W37-2: the role token is stored globally and acts as the connection; the
        // receipt must not attribute it to whichever agent happens to be selected.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.selected = 0;
        app.request_secret(SecretTarget::ForgejoRoleToken);
        app.on_paste("throwaway-role-token");
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.contains("for the connection"), "{}", app.status);
        assert!(!app.status.contains("for `scribe`"), "{}", app.status);
    }

    #[test]
    fn the_finished_count_is_declared_a_window_whenever_older_records_exist() {
        // W36-2: the count is over the newest fifty records. A run in flight inside
        // the window used to turn the `+` off and print the window as exact.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.recent = vec![crate::run::RunSummary {
            agent: "scribe".to_owned(),
            run_id: "run-1-0001-1".to_owned(),
            outcome: None,
            reason: None,
            duration_ms: None,
            started_at: Some("2026-10-15T00:00:00Z".to_owned()),
            read_error: None,
            log: std::path::PathBuf::new(),
            dir: std::path::PathBuf::new(),
        }];
        app.recent_total = 60;
        assert_eq!(
            app.runs_finished_since_start(),
            0,
            "the only windowed run is live"
        );
        assert!(
            app.finished_count_is_capped(),
            "the window was presented as exact"
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

    /// Run one git command in `dir`, failing the test if git refuses.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_workspace_sentence_admits_when_no_identity_is_recorded() {
        // W2-A: before any Forgejo identity is recorded, the sentence compared the
        // repository's local git identity against an identity built from two empty
        // strings and called it "not the agent's" — a claim the product cannot make.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app
            .layout
            .ensure_agent("one")
            .expect("the agent is prepared");
        let workspace = paths.workspace().to_path_buf();
        git(&workspace, &["init", "-q"]);
        git(&workspace, &["config", "user.name", "gewu-one"]);
        git(
            &workspace,
            &["config", "user.email", "gewu-one@agents.invalid"],
        );
        std::fs::write(workspace.join("README"), "hello\n").expect("a file");
        git(&workspace, &["add", "README"]);
        git(&workspace, &["commit", "-q", "-m", "first"]);

        let note = app.workspace_note_for("one");
        assert!(note.contains("no Forgejo identity is recorded"), "{note}");
        assert!(note.contains("cannot say whether it is the agent's"), "{note}");
        assert!(!note.contains("not the agent's"), "{note}");

        // A recorded, matching identity makes the true claim.
        let matching = "[forgejo_logins]\n\"one\" = \"gewu-one\"\n\n[forgejo_emails]\n\"one\" = \"gewu-one@agents.invalid\"\n";
        crate::mode::write_private(app.layout.credentials_file(), matching.as_bytes())
            .expect("credentials");
        let note = app.workspace_note_for("one");
        assert!(
            note.contains("the local git identity is the agent's"),
            "{note}"
        );

        // A recorded identity that differs stays "not the agent's".
        let different = "[forgejo_logins]\n\"one\" = \"someone-else\"\n\n[forgejo_emails]\n\"one\" = \"someone-else@agents.invalid\"\n";
        crate::mode::write_private(app.layout.credentials_file(), different.as_bytes())
            .expect("credentials");
        let note = app.workspace_note_for("one");
        assert!(note.contains("not the agent's"), "{note}");
        assert!(!note.contains("cannot say whether"), "{note}");
    }

    #[test]
    fn a_clone_below_the_workspace_root_is_named_not_denied() {
        // R-2: the product's own clone lands at `workspace/<repo>`, so the root is
        // not a repository after a successful clone. The note names the clone instead
        // of saying the workspace is not a git repository.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app
            .layout
            .ensure_agent("one")
            .expect("the agent is prepared");
        let clone = paths.workspace().join("notes");
        std::fs::create_dir_all(&clone).expect("the clone directory");
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "gewu-one"]);
        git(
            &clone,
            &["config", "user.email", "gewu-one@agents.invalid"],
        );
        std::fs::write(clone.join("README"), "hello\n").expect("a file");
        git(&clone, &["add", "README"]);
        git(&clone, &["commit", "-q", "-m", "first"]);

        let note = app.workspace_note_for("one");
        assert!(note.contains("not a git repository"), "{note}");
        assert!(note.contains("holds 1 cloned repository"), "{note}");
        assert!(note.contains("notes"), "{note}");
        assert!(note.contains("on main"), "{note}");
        // A workspace with no clone keeps the plain sentence.
        let empty = paths.workspace().join("empty");
        std::fs::create_dir_all(&empty).expect("an empty workspace");
        let note = app.workspace_note_for("one");
        assert!(note.contains("notes"), "the clone is still the one on show: {note}");
    }

    #[test]
    fn a_fresh_repository_with_no_commit_is_not_gits_fatal_text() {
        // T-5: a fresh `git init` has no resolvable HEAD; reading it used to draw
        // git's own `fatal: ambiguous argument 'HEAD'` on the Agent screen.
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        let paths = app
            .layout
            .ensure_agent("one")
            .expect("the agent is prepared");
        let workspace = paths.workspace().to_path_buf();
        git(&workspace, &["init", "-q", "-b", "main"]);
        git(&workspace, &["config", "user.name", "gewu-one"]);
        git(
            &workspace,
            &["config", "user.email", "gewu-one@agents.invalid"],
        );

        let note = app.workspace_note_for("one");
        assert!(note.contains("no commit yet"), "{note}");
        assert!(!note.contains("fatal"), "{note}");
        assert!(note.contains("main"), "{note}");
    }
}
