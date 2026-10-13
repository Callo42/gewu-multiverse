//! The state behind the screens, and the keys that move around them.
//!
//! Every key binding is here rather than in the terminal loop, so the screens can be
//! driven without a terminal: the tests feed [`App::on_key`] and render the result.
//!
//! Two rules hold across the whole file. First, a screen never claims a capability
//! the program does not have: the values shown are read from disk, and everything
//! else is named as a placeholder. Second, an agent start is refused unless the
//! first-run risk acknowledgment was confirmed and read back.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};

use crate::config::{AgentConfig, Config};
use crate::credentials::CredentialsFile;
use crate::paths::Layout;
use crate::risk::{self, AckState};

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
    /// The screens in the order the tab bar shows them.
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

/// Everything the screens draw.
#[derive(Debug)]
pub struct App {
    layout: Layout,
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
    /// The one line under the screens.
    pub status: String,
    /// Set when the user asks to leave.
    pub should_quit: bool,
}

impl App {
    /// Build the state and read the files. Nothing here fails: a problem becomes a
    /// message on screen, because the UI has to open even when a file is wrong.
    pub fn new(layout: Layout) -> Self {
        let credentials_path = layout.credentials_file();
        let ack_path = layout.acknowledgment_file();
        let mut app = Self {
            layout,
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
            status: String::new(),
            should_quit: false,
        };
        app.reload();
        // The first-run acknowledgment is put in front of the user on the first start
        // of an installation, that is, whenever no confirmation is recorded.
        app.show_acknowledgment = !app.ack_state.is_confirmed();
        app
    }

    /// Re-read the three files. Run at start and on `r`.
    pub fn reload(&mut self) {
        match self.layout.ensure_root() {
            Ok(()) => {
                self.root_error = None;
                match Config::load_or_write_sample(&self.layout.config_file()) {
                    Ok(config) => {
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
        self.status = self.reload_status();
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

    /// Ask to start the selected agent. Until the risk is acknowledged, the request is
    /// refused; after that, the honest answer is that no agent runs yet.
    pub fn request_agent_start(&mut self) {
        if !self.ack_state.is_confirmed() {
            self.show_acknowledgment = true;
            self.status =
                "refused: no agent may be started before the risk acknowledgment is confirmed"
                    .to_owned();
            return;
        }
        match self.selected_agent() {
            None => {
                self.status = "refused: the configuration lists no agent to start".to_owned();
            }
            Some(agent) => {
                self.status = format!(
                    "not started: running an agent is not implemented in this skeleton, so `{}` was left alone",
                    agent.name
                );
            }
        }
    }

    /// Move a step along the tab bar.
    pub fn shift_screen(&mut self, steps: isize) {
        self.screen = self.screen.shifted(steps);
    }

    /// Show one screen by name.
    pub fn show(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// Move the agent selection, stopping at the ends.
    pub fn move_selection(&mut self, steps: isize) {
        let count = self.agents().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let last = count as isize - 1;
        self.selected = (self.selected as isize + steps).clamp(0, last) as usize;
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
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char(digit) if ('1'..='6').contains(&digit) => {
                let at = digit as usize - '1' as usize;
                self.screen = Screen::ALL[at];
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn app_with(config: &str) -> App {
        let root = scratch_root("app");
        std::fs::write(root.join("config.toml"), config)
            .expect("the test writes the configuration");
        App::new(Layout::rooted_at(root))
    }

    const GOOD: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

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
        let mut first = App::new(Layout::rooted_at(root.clone()));
        first.on_key(key(KeyCode::Char('y')));
        assert!(first.ack_confirmed());
        let second = App::new(Layout::rooted_at(root));
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
        assert!(!app.status.contains("started `one`"));
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
    fn after_the_acknowledgment_a_start_says_plainly_that_nothing_runs_yet() {
        let mut app = app_with(GOOD);
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.ack_confirmed());
        app.on_key(key(KeyCode::Char('s')));
        assert!(app.status.contains("not implemented"), "{}", app.status);
        assert!(app.status.contains("one"), "{}", app.status);
    }

    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut app = app_with(GOOD);
        app.move_selection(-1);
        assert_eq!(app.selected, 0);
        app.move_selection(1);
        assert_eq!(app.selected, 1);
        app.move_selection(5);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn every_screen_is_reachable_and_the_selection_survives_reload() {
        let mut app = app_with(GOOD);
        app.show_acknowledgment = false;
        app.move_selection(1);
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
        // While the dialog is up, the keys behind it do nothing at all.
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
}
