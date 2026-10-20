//! The contract between the board (what the person sees) and the engine (the bundled
//! Forgejo, the agents' accounts and their sessions).
//!
//! The board never touches files, processes or the network: it reads one
//! [`BoardView`] per frame and sends [`Command`]s. The engine never draws. Every
//! command returns at once; slow work (fetching and starting Forgejo, creating
//! accounts, testing the endpoint, starting a session) runs in the background and
//! shows up in the next [`BoardView`].

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Where the bundled Forgejo is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgejoState {
    /// The pinned Forgejo binary is not on this machine yet.
    Missing,
    /// Downloading the pinned binary; `percent` is 0–100 when the size is known.
    Fetching { percent: Option<u8> },
    /// Writing its configuration, creating its database and the person's account.
    Preparing,
    /// The process is up; waiting for it to answer.
    Starting,
    /// Answering on [`ForgejoView::url`].
    Running,
    /// Not running (the program is closing, or it was never started).
    Stopped,
    /// It could not be fetched, prepared or started; `reason` is one short sentence.
    Failed { reason: String },
}

/// The one Forgejo every agent is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoView {
    pub state: ForgejoState,
    /// The address the person opens in a browser, e.g. `http://127.0.0.1:3000`.
    pub url: String,
    /// The pinned Forgejo version, e.g. `17.0.0`.
    pub version: String,
    /// The person's own administrator login on it.
    pub admin_login: String,
    /// The organisation every agent belongs to, e.g. `team`.
    pub organisation: String,
    /// Repositories in that organisation (the agents' shared work).
    pub repositories: usize,
}

/// Whether an agent's Forgejo account is ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioning {
    /// Waiting for Forgejo, or its account is being created.
    Pending,
    /// The account exists, its token is stored, it belongs to the organisation.
    Ready,
    /// The account could not be created; `reason` is one short sentence.
    Failed { reason: String },
}

/// What the person asked an agent to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desired {
    Started,
    Stopped,
}

/// How one session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    Succeeded,
    Failed { reason: String },
    TimedOut,
    Stopped,
}

/// What an agent is doing now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Stopped: no session runs and none will start.
    Idle,
    /// A session is running; `number` counts sessions since the agent was last started.
    Running { number: u64, since: SystemTime },
    /// Started and between sessions: the next one starts at `next_at`.
    Restarting { next_at: SystemTime, last: SessionEnd },
    /// Started but unable to run; `reason` is one short sentence naming the fix
    /// (no endpoint, an empty instruction, Forgejo not running, the account not ready).
    Blocked { reason: String },
}

/// One agent, as its card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    /// Its name, e.g. `agent-1`.
    pub name: String,
    /// Its own Forgejo account, e.g. `gewu-agent-1`.
    pub login: String,
    pub provisioning: Provisioning,
    pub desired: Desired,
    pub session: SessionState,
    /// Sessions since the agent was last started.
    pub sessions_since_start: u64,
    /// How the newest finished sessions ended, newest last, at most ten.
    pub recent: Vec<SessionEnd>,
    /// The instruction's length in characters; 0 means none is written yet.
    pub instruction_chars: usize,
    /// The first line of the instruction, for the card and the panel.
    pub instruction_first_line: Option<String>,
    /// The newest thing it did in Forgejo, short: `commented #12`, `opened PR #4`.
    pub activity: Option<String>,
}

/// How an endpoint test went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointTest {
    NotRun,
    Running,
    Passed { answer: String },
    Failed { reason: String },
}

/// The one model endpoint every agent uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointView {
    pub configured: bool,
    /// The completions URL as written.
    pub url: String,
    pub model: String,
    pub key_present: bool,
    pub test: EndpointTest,
}

/// Everything the board shows, read once per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardView {
    /// The first-run terms were acknowledged (agents cannot start before).
    pub terms_acknowledged: bool,
    /// The terms text, one short item per entry, for the first-run screen.
    pub terms: Vec<String>,
    /// Endpoint set and at least one agent exists: the setup steps are done.
    pub setup_complete: bool,
    pub forgejo: ForgejoView,
    pub agents: Vec<AgentView>,
    pub endpoint: EndpointView,
    /// The outcome of the newest command, one short line, shown once.
    pub notice: Option<String>,
    /// The administrator password, present only in the view right after
    /// [`Command::RevealAdminPassword`], and never again until asked.
    pub admin_password: Option<String>,
}

/// What the board can ask for. Every command returns at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Record the first-run terms.
    AcknowledgeTerms,
    /// Set the one model endpoint; `key: None` keeps the stored key.
    SetEndpoint { url: String, model: String, key: Option<String> },
    /// Test the endpoint in the background.
    TestEndpoint,
    /// First setup: create `agent-1` … `agent-N` and their accounts.
    CreateAgents { count: usize },
    /// Add one more agent (the next free `agent-N`).
    AddAgent,
    /// Stop it, revoke its token, remove it from the organisation; its Forgejo account
    /// and its history stay.
    RemoveAgent { name: String },
    Start { name: String },
    Stop { name: String },
    /// Fetch (if needed), prepare and start the bundled Forgejo.
    StartForgejo,
    /// Put the administrator password into the next view only.
    RevealAdminPassword,
}

/// The engine as the board uses it.
pub trait EngineApi {
    /// The whole picture; cheap enough to call every frame.
    fn board(&mut self) -> BoardView;
    /// Queue a command; returns at once. `Err` is one short sentence for the notice line.
    fn send(&mut self, command: Command) -> Result<(), String>;
    /// Advance background work: reap ended sessions, start due ones, follow Forgejo.
    /// The board calls it on every tick (about four times a second).
    fn tick(&mut self);
    /// The file the person edits for an agent's instruction (the board hands it to `$EDITOR`).
    fn instruction_path(&self, agent: &str) -> Option<PathBuf>;
    /// The newest session's readable log and raw transcript, for the pager hand-off.
    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)>;
    /// Stop every session and the Forgejo; wait at most `grace` before forcing.
    fn shutdown(&mut self, grace: Duration);
}
