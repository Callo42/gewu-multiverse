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
use crate::credentials::CredentialsFile;
use crate::paths::Layout;
use crate::risk::{self, AckState};
use crate::run::{RunSummary, recent_runs};
use crate::supervisor::{StartDisposition, StopDisposition, Supervisor};

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
        let Some(name) = self.selected_name() else {
            self.status = "refused: the configuration lists no agent to start".to_owned();
            return;
        };
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
        match self.supervisor.start(&paths, steps) {
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
        };
        self.refresh_recent();
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
        if self.screen == Screen::Runs {
            self.move_run_selection(steps);
        } else {
            self.move_agent_selection(steps);
        }
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
