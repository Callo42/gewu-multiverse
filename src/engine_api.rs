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
    /// Their `team/name` names, sorted, for the Forgejo page.
    pub repository_names: Vec<String>,
    /// Repository changes seen between two administration-API polls, oldest first:
    /// `team/scratch was deleted`, `team/website was created`.
    pub events: Vec<String>,
    /// The recorded login is one an earlier version made, not the fixed default.
    pub admin_login_kept: bool,
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
    /// The run exited cleanly, but its own final summary says it could not do the
    /// instruction. It is not a success on the card.
    Refused {
        reason: String,
    },
    /// The run exited cleanly and did nothing, and its own summary says there was
    /// nothing assigned to do. It is not a success on the card.
    NoWork {
        reason: String,
    },
    /// The run exited cleanly but a tool call failed (a non-zero exit or a signal)
    /// and was not a refused write. It is not a success on the card; the reason is
    /// the failed command's cause (correction 4).
    Warned {
        reason: String,
    },
    Failed {
        reason: String,
    },
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
    Restarting {
        next_at: SystemTime,
        last: SessionEnd,
    },
    /// Started but unable to run; `reason` is one short sentence naming the fix
    /// (no endpoint, an empty instruction, Forgejo not running, the account not ready).
    Blocked { reason: String },
}

/// One native program the product can install, as the Program screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramView {
    /// Stable id (`pi`, `codex`, `dsh`, `opencode`).
    pub id: String,
    /// Display name.
    pub label: String,
    /// The tested version.
    pub version: String,
    /// The actual installed version, when installed.
    pub installed_version: Option<String>,
    /// Whether the executable is present.
    pub installed: bool,
    /// The official licence.
    pub license: String,
    /// Approximate download size in MB.
    pub download_mb: u32,
    /// A non-Rust prerequisite sentence, when the program has one.
    pub prerequisite: Option<String>,
    /// Whether the prerequisite is met on this computer right now. The Program
    /// screen keeps the exact versions and the `i` action when it is not.
    pub prerequisite_met: bool,
    /// An install in progress, 0-100 when known.
    pub installing: Option<Option<u8>>,
    /// The current install phase word (`downloading`, `installing packages`, …).
    pub installing_phase: Option<String>,
    /// The newest install failure, one sentence.
    pub error: Option<String>,
}

/// The immutable program facts of one run, for active-versus-next display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProgramView {
    /// Program id.
    pub program: String,
    /// Program display name.
    pub label: String,
    /// Actual native version, or the product version for built-in.
    pub version: String,
    /// Connection protocol label.
    pub protocol: String,
    /// Public provider address, without credentials.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Codex sandbox mode (`native` or `full-access`), when relevant.
    pub codex_sandbox: Option<String>,
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
    /// Sessions since the agent was last started (the session counter; the card's
    /// `run N` is the total below, which does not reset).
    pub sessions_since_start: u64,
    /// Every run recorded for this agent, reconstructed from the records on open and
    /// updated as runs end. It does not reset when the agent starts or the program
    /// reopens (W31-5, F5).
    pub runs_total: u64,
    /// How the newest finished sessions ended, newest last, at most ten.
    pub recent: Vec<SessionEnd>,
    /// The instruction's length in characters; 0 means none is written yet.
    pub instruction_chars: usize,
    /// The first line of the instruction, for the card and the panel.
    pub instruction_first_line: Option<String>,
    /// The whole instruction, so a tall panel can show more than its first line
    /// without the board reading a file (U4/F-11). None when there is none.
    pub instruction: Option<String>,
    /// How many lines the whole instruction has, so a card can say how much it is
    /// not showing.
    pub instruction_lines: usize,
    /// The newest thing it did in Forgejo, short: `commented #12`, `opened PR #4`.
    pub activity: Option<String>,
    /// When that activity happened, for `last: … · 2m ago`.
    pub activity_at: Option<SystemTime>,
    /// The last finished run's error, first clause only, for the card.
    pub last_error: Option<String>,
    /// The newest clean run's own final summary; the card shows it beside the run
    /// mark, so `✓` carries what the run says it did as well as `⊘`/`∅` do.
    pub last_result: Option<String>,
    /// The last failure of a tool call inside the current or newest run: one line,
    /// `shell: exit 128 — …`'s first clause; the card adds `⚠` or `✗`.
    pub last_failure: Option<String>,
    /// A left-over process group from an earlier run that the engine could not
    /// stop. The card says so instead of hiding it (F-02/F-03).
    pub leftover: Option<String>,
    /// The tool call in flight right now, if a run is inside one.
    pub current_tool: Option<String>,
    /// How many seconds that call has been in flight.
    pub current_tool_seconds: u64,
    /// A temporary model failure the running program is retrying, in the provider's
    /// own words. It is not a failed run: the card says it is retrying and keeps the
    /// working state until the program either recovers or gives up.
    pub retrying: Option<String>,
    /// The last issue, branch, pull request and comment named by the newest run that
    /// did work, at most three short typed lines, for the card's work block. It stays
    /// until a later run does new work.
    pub last_work: Vec<String>,
    /// The newest run's own log says no open issue was assigned to this agent. The
    /// Agent screen names the fix instead of leaving an empty work block.
    pub no_assigned_work: bool,
    /// The newest finished run that did actual work, and its own final summary. The
    /// card prefers it over a later no-op's reason, so the persistent work block is
    /// reflected in the one-line summary (F-10).
    pub last_work_result: Option<String>,
    /// Whether the newest finished run itself did work. False means a later result
    /// is a check run and must not be presented as the agent's whole work.
    pub last_result_is_work: bool,
    /// The selected program id (`builtin` for the legacy engine).
    pub program: String,
    /// The selected program's display name.
    pub program_label: String,
    /// A saved program value this build does not know, kept for repair.
    pub unknown_program: Option<String>,
    /// Whether the selected program is installed.
    pub program_installed: bool,
    /// The installed version, when it is.
    pub program_version: Option<String>,
    /// The selected connection protocol word.
    pub protocol: String,
    /// The selected API base URL.
    pub base_url: String,
    /// The selected model id.
    pub model: String,
    /// Whether a key is available for this agent (its own or the shared one).
    pub key_present: bool,
    /// The newest per-agent connection test, when one was run on this form.
    pub model_test: EndpointTest,
    /// Why Start would refuse this agent's program/connection, in one sentence.
    pub program_error: Option<String>,
    /// The actual program facts of the newest run, when it has one.
    pub active_program: Option<RunProgramView>,
    /// The saved facts the next run will use, when they differ from the active run.
    pub next_program: Option<RunProgramView>,
    /// The selected Codex sandbox mode (`native`/`full-access`), when Codex.
    pub codex_sandbox: Option<String>,
}

/// One agent's kept program and model, named on the upgrade screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeAgent {
    /// The agent's name.
    pub name: String,
    /// The kept program's display name.
    pub program: String,
    /// The kept model id.
    pub model: String,
    /// The kept connection type's display name.
    pub protocol: String,
}

/// What an older installation looked like when this version first opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeView {
    /// The version recorded by the earlier build, e.g. `0.1.29`.
    pub from_version: String,
    /// This build's version.
    pub to_version: String,
    /// The earlier settings file was found; its model and agents are reused when
    /// `multiverse.toml` exists, and `config.toml` is left as it was.
    pub settings: bool,
    /// How many agents the settings name.
    pub agents: usize,
    /// How many instruction documents were found.
    pub instructions: usize,
    /// How many run records were found.
    pub runs: usize,
    /// An old `config.toml` was found and is ignored (never read or converted).
    pub ignored_config: bool,
    /// The programs and per-agent connections found and kept, in agent order.
    pub kept_agents: Vec<UpgradeAgent>,
    /// How many installed programs were found by their install records.
    pub programs: usize,
    /// The release page address.
    pub release_url: String,
    /// One line from this release's notes naming what changed.
    pub whats_new: String,
    /// The first-run warnings differ from this installation's recorded wording and
    /// will be shown again after `⏎`.
    pub warnings_changed: bool,
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
    /// The completions URL as written (legacy).
    pub url: String,
    /// The API base URL for a new connection.
    pub base_url: String,
    /// The connection protocol word.
    pub protocol: String,
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
    /// The native programs and their installed state.
    pub programs: Vec<ProgramView>,
    /// The outcome of the newest command, one short line, shown once.
    pub notice: Option<String>,
    /// The administrator password, present only in the view right after
    /// [`Command::RevealAdminPassword`], and never again until asked.
    pub admin_password: Option<String>,
    /// Whether this computer can confine an agent's shell. When it cannot, the
    /// board and the warnings say `runs are not confined on this computer`.
    pub confined: bool,
    /// The mechanism this computer really enforces, for the warnings screen's
    /// positive sentence: `Some("Landlock")` reads
    /// `This computer confines runs (Landlock)` (F-06). `None` on a host
    /// with no mechanism; the negative sentence then comes from `confined`.
    pub confinement: Option<String>,
    /// The one screen shown the first time this build opens an older installation.
    pub upgrade: Option<UpgradeView>,
}

/// What the board can ask for. Every command returns at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Record the first-run terms.
    AcknowledgeTerms,
    /// Set the default model endpoint; `key: None` keeps the stored key.
    SetEndpoint {
        url: String,
        model: String,
        key: Option<String>,
    },
    /// Set the default connection in the new base-URL/protocol shape.
    SetConnection {
        base_url: String,
        protocol: String,
        model: String,
        key: Option<String>,
    },
    /// Select one agent's program. Native selection snapshots the default
    /// connection the first time; it never changes another agent.
    SetAgentProgram {
        name: String,
        program: String,
        /// The selected connection type for the new program, when the person chose
        /// one. `None` keeps a compatible existing provider protocol or the
        /// program's default.
        protocol: Option<String>,
    },
    /// Set one agent's explicit connection and key; `key: None` keeps its key.
    SetAgentModel {
        name: String,
        base_url: String,
        protocol: String,
        model: String,
        key: Option<String>,
    },
    /// Set the explicit Codex compatibility sandbox choice for one agent.
    SetAgentCodexSandbox {
        name: String,
        mode: String,
    },
    /// Install one native program into the product's data directory.
    InstallProgram {
        id: String,
    },
    /// Stop one in-flight install, keeping the previous usable version.
    CancelInstallProgram {
        id: String,
    },
    /// Test one agent's selected model with one real request.
    TestAgentModel {
        name: String,
    },
    /// Test the endpoint in the background.
    TestEndpoint,
    /// First setup: create `agent-1` … `agent-N` and their accounts.
    CreateAgents {
        count: usize,
    },
    /// Add one more agent (the next free `agent-N`).
    AddAgent,
    /// Stop it, revoke its token, remove it from the organisation; its Forgejo account
    /// and its history stay.
    RemoveAgent {
        name: String,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
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
    /// The newest session's human-readable transcript, written beside the JSONL.
    /// A test engine may not have one; the board then shows the JSONL itself.
    fn readable_transcript(&self, agent: &str) -> Option<PathBuf> {
        let _ = agent;
        None
    }
    /// Stop every session and the Forgejo; wait at most `grace` before forcing.
    fn shutdown(&mut self, grace: Duration);
}
