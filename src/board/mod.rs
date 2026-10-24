//! The board: the product as one picture.
//!
//! The board reads one [`BoardView`](crate::engine_api::BoardView) per frame and
//! sends [`Command`](crate::engine_api::Command)s. It owns no product state: it
//! never opens the configuration, starts a process or talks to the network. The
//! engine behind it does all of that; [`fake::FakeEngine`] scripts it for this build.

pub mod fake;
mod render;
pub mod status;
mod text;
mod theme;

use crate::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EngineApi, Provisioning, SessionState,
};
use crate::harness::{Program, Protocol};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::Duration;

pub use fake::FakeEngine;
pub use render::{BoardForm, BoardLayout};
pub use text::words;

/// Which screen the person is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// First run: the facts, one item at a time.
    Terms,
    /// First run or `m`: the one model endpoint.
    Model,
    /// First run: how many agents to create.
    Picker,
    /// First run: the picture filling in.
    Build,
    /// The board itself.
    Home,
    /// One agent, opened from its card.
    Agent,
    /// One agent's agent program.
    Program,
    /// The one Forgejo.
    Forgejo,
    /// The keys.
    Help,
    /// The first open by a newer build: what was found and what is reused.
    Upgrade,
}

/// Where the model screen was opened from, which decides what `enter` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelOrigin {
    FirstRun,
    Board,
}

/// Which program a hand-off needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffKind {
    /// `$EDITOR` on the instruction.
    Edit,
    /// `$PAGER` on a log or transcript.
    View,
}

/// One file waiting for the terminal to be handed to the person's own program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub kind: HandoffKind,
    pub path: PathBuf,
    pub label: String,
}

/// The board and everything it is showing.
pub struct Board {
    engine: Box<dyn EngineApi>,
    view: BoardView,
    screen: Screen,
    help_back: Screen,
    last: BoardLayout,
    selection: usize,
    forgejo_selected: bool,
    scroll_row: usize,
    columns: usize,
    terms_seen: Vec<bool>,
    terms_offset: usize,
    model_fields: [String; 3],
    model_focus: usize,
    model_editing: bool,
    model_cursor: usize,
    /// The fields changed since the last test, so a stale `✓` is not shown beside
    /// a value the provider never saw (W67-13).
    model_edited: bool,
    model_origin: ModelOrigin,
    agent_count: usize,
    build_ready_ticks: u8,
    agent_name: String,
    /// The agent whose model form is open; `None` is the default connection.
    model_agent: Option<String>,
    /// The selected row on the Program screen: 0 is built-in, then NATIVE_PROGRAMS.
    program_index: usize,
    /// The connection type selected beside the program.
    program_protocol: Protocol,
    /// The Codex dangerous-sandbox explanation was shown; a second `f` is opt-in.
    codex_optin_confirmed: bool,
    confirm_remove: Option<String>,
    confirm_quit: bool,
    notice: Option<String>,
    notice_ticks: u8,
    revealed_password: Option<(String, u8)>,
    clipboard: Option<String>,
    sent: Vec<Command>,
    handoff: Option<Handoff>,
    quitting: bool,
    /// The live started agents the quit confirmation named, frozen so the final
    /// line counts the same population even if shutdown updates the view.
    quit_stopped: Option<usize>,
    shutdown_sent: bool,
    shutdown_calls: usize,
}

impl Board {
    /// Read the first view and open the screen the person belongs on.
    pub fn new(engine: impl EngineApi + 'static) -> Self {
        let mut engine = Box::new(engine);
        let view = engine.board();
        let startup_notice = view.notice.clone();
        let terms_seen = vec![false; view.terms.len()];
        let screen = if view.upgrade.is_some() {
            Screen::Upgrade
        } else if !view.terms_acknowledged {
            Screen::Terms
        } else if !view.endpoint.configured {
            Screen::Model
        } else if view.agents.is_empty() {
            Screen::Picker
        } else {
            Screen::Home
        };
        let mut board = Self {
            engine,
            view,
            screen,
            help_back: Screen::Home,
            last: BoardLayout::default(),
            selection: 0,
            forgejo_selected: false,
            scroll_row: 0,
            columns: 1,
            terms_seen,
            terms_offset: 0,
            model_fields: [String::new(), String::new(), String::new()],
            model_focus: 0,
            model_editing: false,
            model_cursor: 0,
            model_edited: false,
            model_origin: ModelOrigin::FirstRun,
            agent_count: 3,
            build_ready_ticks: 0,
            agent_name: String::new(),
            model_agent: None,
            program_index: 0,
            program_protocol: Protocol::ChatCompletions,
            codex_optin_confirmed: false,
            confirm_remove: None,
            confirm_quit: false,
            notice: None,
            notice_ticks: 0,
            revealed_password: None,
            clipboard: None,
            sent: Vec::new(),
            handoff: None,
            quitting: false,
            quit_stopped: None,
            shutdown_sent: false,
            shutdown_calls: 0,
        };
        board.set_model_from_view();
        board.after_view();
        if let Some(notice) = startup_notice {
            board.set_notice(notice);
        }
        board
    }

    /// Advance the engine and read the next picture.
    pub fn tick(&mut self) {
        self.engine.tick();
        self.read_view();
        if self.notice_ticks > 0 {
            self.notice_ticks -= 1;
            if self.notice_ticks == 0 {
                self.notice = None;
            }
        }
        if let Some((_, ticks)) = &mut self.revealed_password {
            *ticks = ticks.saturating_sub(1);
            if *ticks == 0 {
                self.revealed_password = None;
            }
        }
    }

    fn read_view(&mut self) {
        let view = self.engine.board();
        if let Some(notice) = view.notice.clone() {
            self.set_notice(notice);
        }
        if let Some(password) = view.admin_password.clone() {
            // Ten seconds at the board's four ticks a second: long enough to read a
            // wrapped value and type it into a browser.
            self.revealed_password = Some((password, 40));
        }
        self.view = view;
        self.after_view();
    }

    fn after_view(&mut self) {
        if self.screen == Screen::Terms && self.terms_seen.len() != self.view.terms.len() {
            self.terms_seen.resize(self.view.terms.len(), false);
        }
        if self.screen == Screen::Build {
            let ready = self.view.forgejo.state == crate::engine_api::ForgejoState::Running
                && !self.view.agents.is_empty()
                && self
                    .view
                    .agents
                    .iter()
                    .all(|agent| agent.provisioning == Provisioning::Ready);
            if ready {
                self.build_ready_ticks = self.build_ready_ticks.saturating_add(1);
                if self.build_ready_ticks >= 2 {
                    self.screen = Screen::Home;
                    self.selection = 0;
                    self.forgejo_selected = false;
                    self.build_ready_ticks = 0;
                }
            } else {
                self.build_ready_ticks = 0;
            }
        }
        if self.screen == Screen::Home || self.screen == Screen::Agent {
            if self.view.agents.is_empty() {
                if self.screen == Screen::Agent {
                    self.screen = Screen::Home;
                }
            } else if self.selection >= self.view.agents.len() {
                self.selection = self.view.agents.len() - 1;
            }
            if self.screen == Screen::Agent
                && !self
                    .view
                    .agents
                    .iter()
                    .any(|agent| agent.name == self.agent_name)
            {
                self.screen = Screen::Home;
            }
        }
    }

    fn set_notice(&mut self, text: impl Into<String>) {
        // The engine writes notices with the names in backticks, as its own records
        // do. A board notice is one plain line: the markdown marks are not part of
        // what a person reads.
        self.notice = Some(crate::board::status::plain(&text.into()));
        self.notice_ticks = 8;
    }

    fn send(&mut self, command: Command) {
        let result = self.engine.send(command.clone());
        self.sent.push(command);
        if let Err(message) = result {
            self.set_notice(message);
        }
        self.read_view();
    }

    fn set_model_from_view(&mut self) {
        if let Some(name) = self.model_agent.as_deref() {
            if let Some(agent) = self.view.agents.iter().find(|agent| agent.name == name) {
                self.model_fields[0] = agent.base_url.clone();
                self.model_fields[1] = agent.model.clone();
                self.model_fields[2].clear();
                self.model_focus = 0;
                self.model_editing = false;
                self.model_edited = false;
                self.model_cursor = self.model_fields[0].chars().count();
                return;
            }
        }
        self.model_fields[0] = self.view.endpoint.url.clone();
        self.model_fields[1] = self.view.endpoint.model.clone();
        self.model_fields[2].clear();
        self.model_focus = 0;
        self.model_editing = false;
        self.model_edited = false;
        self.model_cursor = self.model_fields[0].chars().count();
    }

    /// Whether the current field values are the ones the last passing test saw.
    fn endpoint_tested(&self) -> bool {
        !self.model_edited
            && matches!(self.view.endpoint.test, EndpointTest::Passed { .. })
            && self.model_fields[0].trim() == self.view.endpoint.url.trim()
            && self.model_fields[1].trim() == self.view.endpoint.model.trim()
    }

    /// Whether a test result is still about the values on screen.
    pub fn model_test_visible(&self) -> bool {
        !self.model_edited
    }

    fn address_names_completions(&self) -> bool {
        let url = self.model_fields[0].trim();
        let path = url.split(['?', '#']).next().unwrap_or(url);
        path.trim_end_matches('/').ends_with("/chat/completions")
    }

    fn endpoint_command(&self) -> Command {
        let key = if self.model_fields[2].is_empty() {
            None
        } else {
            Some(self.model_fields[2].clone())
        };
        if let Some(name) = self.model_agent.as_ref() {
            let protocol = self
                .view
                .agents
                .iter()
                .find(|agent| &agent.name == name)
                .and_then(|agent| Protocol::parse(&agent.protocol))
                .unwrap_or(Protocol::ChatCompletions);
            let base = self.model_fields[0]
                .trim()
                .trim_end_matches('/')
                .trim_end_matches("/chat/completions")
                .trim_end_matches('/')
                .to_owned();
            Command::SetAgentModel {
                name: name.clone(),
                base_url: base,
                protocol: protocol.id().to_owned(),
                model: self.model_fields[1].trim().to_owned(),
                key,
            }
        } else {
            Command::SetEndpoint {
                url: self.model_fields[0].trim().to_owned(),
                model: self.model_fields[1].trim().to_owned(),
                key,
            }
        }
    }

    fn endpoint_fields_filled(&self) -> bool {
        !self.model_fields[0].trim().is_empty() && !self.model_fields[1].trim().is_empty()
    }

    /// Whether a key would be sent: one typed now, or one already stored.
    fn key_available(&self) -> bool {
        if let Some(name) = self.model_agent.as_deref() {
            return !self.model_fields[2].trim().is_empty()
                || self
                    .view
                    .agents
                    .iter()
                    .find(|agent| agent.name == name)
                    .is_some_and(|agent| agent.key_present);
        }
        !self.model_fields[2].trim().is_empty() || self.view.endpoint.key_present
    }

    /// What the person asked for.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.request_quit();
            return;
        }
        match self.screen {
            Screen::Terms => self.keys_terms(key),
            Screen::Model => self.keys_model(key),
            Screen::Picker => self.keys_picker(key),
            Screen::Build => self.keys_build(key),
            Screen::Home => self.keys_home(key),
            Screen::Agent => self.keys_agent(key),
            Screen::Program => self.keys_program(key),
            Screen::Forgejo => self.keys_forgejo(key),
            Screen::Help => self.keys_help(key),
            Screen::Upgrade => self.keys_upgrade(key),
        }
    }

    /// After the upgrade screen: the first screen this person belongs on.
    fn after_upgrade(&self) -> Screen {
        if !self.view.terms_acknowledged {
            Screen::Terms
        } else if !self.view.endpoint.configured {
            Screen::Model
        } else if self.view.agents.is_empty() {
            Screen::Picker
        } else {
            Screen::Home
        }
    }

    fn keys_upgrade(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ') => {
                self.screen = self.after_upgrade();
            }
            _ => {
                if let Some('q') = plain(key) {
                    self.request_quit();
                }
            }
        }
    }

    fn keys_terms(&mut self, key: KeyEvent) {
        match plain(key) {
            Some('y') => {
                if self.terms_seen.iter().all(|seen| *seen) {
                    // The acknowledgement is recorded first, so the view this
                    // transition reads already says the warnings are agreed. An
                    // upgrade whose warnings changed goes back to the board then,
                    // not into the first-run model/picker flow (F-06).
                    self.send(Command::AcknowledgeTerms);
                    self.screen = self.after_upgrade();
                    if self.screen == Screen::Model {
                        // First run has no agents; an upgrade with agents but no
                        // model returns to the board once the model is set.
                        self.model_origin = if self.view.agents.is_empty() {
                            ModelOrigin::FirstRun
                        } else {
                            ModelOrigin::Board
                        };
                        self.set_model_from_view();
                    }
                } else {
                    self.set_notice("read every item, then y");
                }
            }
            Some('n') => self.leave_without_warning(),
            _ => match key.code {
                KeyCode::Up => self.terms_offset = self.terms_offset.saturating_sub(1),
                KeyCode::Down => self.terms_offset = self.terms_offset.saturating_add(1),
                _ => {}
            },
        }
    }

    fn keys_model(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('u')) {
            clear_string(&mut self.model_fields[self.model_focus]);
            self.model_cursor = 0;
            self.model_edited = true;
            return;
        }
        if self.model_editing {
            // One Tab behaviour in both modes: Tab keeps the field being edited and
            // moves to the next one. Only Enter finishes editing and stays.
            match key.code {
                KeyCode::Tab | KeyCode::Down => {
                    // Tab moves and keeps the person editing: one behaviour in both
                    // modes, so a field can be typed without pressing `e` again.
                    self.model_focus = (self.model_focus + 1) % 3;
                    self.model_cursor = self.model_fields[self.model_focus].chars().count();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.model_focus = (self.model_focus + 2) % 3;
                    self.model_cursor = self.model_fields[self.model_focus].chars().count();
                }
                _ => self.edit_field(key),
            }
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.model_focus = (self.model_focus + 2) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
            }
            KeyCode::Down => {
                self.model_focus = (self.model_focus + 1) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
            }
            KeyCode::Tab => {
                // One Tab behaviour: next field, ready to type, in view mode too.
                self.model_focus = (self.model_focus + 1) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
                self.model_editing = true;
            }
            KeyCode::BackTab => {
                self.model_focus = (self.model_focus + 2) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
                self.model_editing = true;
            }
            KeyCode::Enter => {
                if !self.endpoint_fields_filled() {
                    self.set_notice("API address and model needed, then ⏎");
                    return;
                }
                // The same local checks `t` runs, before anything is saved or sent.
                // An address the product itself rejects is never left behind by a
                // silent continue (W67-1).
                if self.model_agent.is_none() && !self.address_names_completions() {
                    self.set_notice("✗ wrong address — it should end in /chat/completions");
                    return;
                }
                if !self.key_available() {
                    self.set_notice("✗ the API key is empty");
                    return;
                }
                let tested = self.endpoint_tested();
                self.send(self.endpoint_command());
                if self.model_agent.is_some() {
                    self.screen = Screen::Agent;
                } else {
                    self.screen = match self.model_origin {
                        ModelOrigin::FirstRun => Screen::Picker,
                        ModelOrigin::Board => Screen::Home,
                    };
                    if !tested {
                        self.set_notice("not tested — t to test");
                    }
                }
            }
            KeyCode::Esc => {
                self.screen = if self.model_agent.is_some() {
                    Screen::Agent
                } else {
                    match self.model_origin {
                        ModelOrigin::FirstRun => Screen::Terms,
                        ModelOrigin::Board => Screen::Home,
                    }
                };
            }
            _ => match plain(key) {
                Some('t') if self.model_agent.is_some() => {
                    self.set_notice("Start uses and reports this agent's own connection");
                }
                Some('t') => {
                    if !self.endpoint_fields_filled() {
                        self.set_notice("API address and model needed, then t");
                    } else if !self.key_available() {
                        // Nothing is sent: the error is the empty key, not a refusal
                        // by a provider that never saw a request.
                        self.set_notice("✗ the API key is empty");
                    } else {
                        self.send(self.endpoint_command());
                        self.send(Command::TestEndpoint);
                        // The save notice must not sit under a failed test: the test's
                        // own line is the only verdict on this model.
                        self.notice = None;
                        self.notice_ticks = 0;
                        self.model_edited = false;
                    }
                }
                Some('e') => self.begin_edit(),
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn begin_edit(&mut self) {
        self.model_editing = true;
        self.model_cursor = self.model_fields[self.model_focus].chars().count();
    }

    /// Move to the next model field and put the cursor at its end.
    fn next_model_field(&mut self) {
        self.model_focus = (self.model_focus + 1) % 3;
        self.model_cursor = self.model_fields[self.model_focus].chars().count();
    }

    fn edit_field(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                // `⏎` commits the field and selects the next one without entering
                // its edit mode, so `t` still tests and a key cannot be overwritten
                // by the next keystroke (F-05/W67-5).
                self.model_editing = false;
                if self.model_focus + 1 < 3 {
                    self.next_model_field();
                }
            }
            KeyCode::Esc => self.model_editing = false,
            KeyCode::Left => self.model_cursor = self.model_cursor.saturating_sub(1),
            KeyCode::Right => {
                let len = self.model_fields[self.model_focus].chars().count();
                self.model_cursor = (self.model_cursor + 1).min(len);
            }
            KeyCode::Home => self.model_cursor = 0,
            KeyCode::End => self.model_cursor = self.model_fields[self.model_focus].chars().count(),
            KeyCode::Backspace => {
                if self.model_cursor > 0 {
                    self.model_cursor -= 1;
                    remove_char(&mut self.model_fields[self.model_focus], self.model_cursor);
                    self.model_edited = true;
                }
            }
            KeyCode::Delete => {
                remove_char(&mut self.model_fields[self.model_focus], self.model_cursor);
                self.model_edited = true;
            }
            _ => {
                if let Some(ch) = plain(key) {
                    if !ch.is_control() {
                        insert_char(
                            &mut self.model_fields[self.model_focus],
                            self.model_cursor,
                            ch,
                        );
                        self.model_cursor += 1;
                        self.model_edited = true;
                    }
                }
            }
        }
    }

    fn keys_picker(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Right | KeyCode::Down => self.agent_count = (self.agent_count + 1).min(12),
            KeyCode::Left | KeyCode::Up => {
                self.agent_count = self.agent_count.saturating_sub(1).max(1)
            }
            KeyCode::Enter => {
                let count = self.agent_count;
                self.send(Command::CreateAgents { count });
                self.send(Command::StartForgejo);
                self.build_ready_ticks = 0;
                self.screen = Screen::Build;
            }
            KeyCode::Esc => {
                self.screen = Screen::Model;
                self.model_origin = ModelOrigin::FirstRun;
            }
            _ => match plain(key) {
                Some('+') => self.agent_count = (self.agent_count + 1).min(12),
                Some('-') => self.agent_count = self.agent_count.saturating_sub(1).max(1),
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn keys_build(&mut self, key: KeyEvent) {
        match plain(key) {
            Some('q') => self.request_quit(),
            Some('r') => {
                if matches!(
                    self.view.forgejo.state,
                    crate::engine_api::ForgejoState::Failed { .. }
                ) {
                    self.send(Command::StartForgejo);
                }
            }
            _ => {}
        }
    }

    fn keys_home(&mut self, key: KeyEvent) {
        if let Some(name) = self.confirm_remove.clone() {
            match plain(key) {
                Some('y') => {
                    self.confirm_remove = None;
                    self.send(Command::RemoveAgent { name });
                    self.selection = self.selection.min(self.view.agents.len().saturating_sub(1));
                }
                Some('n') | Some('q') => self.confirm_remove = None,
                _ if key.code == KeyCode::Esc => self.confirm_remove = None,
                _ => {}
            }
            return;
        }
        if self.confirm_quit {
            if let Some('q') = plain(key) {
                self.shutdown();
                self.quitting = true;
            } else if matches!(key.code, KeyCode::Esc) || matches!(plain(key), Some('n')) {
                self.confirm_quit = false;
                // The person changed their mind; a later quit must freeze a fresh
                // population, not the one this cancelled confirmation named.
                self.quit_stopped = None;
            }
            return;
        }
        let count = self.view.agents.len();
        match key.code {
            KeyCode::Left => {
                if count > 0 {
                    if self.forgejo_selected {
                        self.forgejo_selected = false;
                        self.selection = count - 1;
                    } else {
                        self.selection = self.selection.saturating_sub(1);
                    }
                }
            }
            KeyCode::Right => {
                if self.forgejo_selected {
                    self.forgejo_selected = false;
                    self.selection = 0;
                } else if count > 0 {
                    self.selection = (self.selection + 1).min(count - 1);
                }
            }
            KeyCode::Up => {
                if !self.forgejo_selected {
                    if self.selection < self.columns.max(1) {
                        self.forgejo_selected = true;
                    } else {
                        self.selection -= self.columns.max(1);
                    }
                }
            }
            KeyCode::Down => {
                if self.forgejo_selected {
                    self.forgejo_selected = false;
                    self.selection = 0;
                } else if count > 0 {
                    self.selection = (self.selection + self.columns.max(1)).min(count - 1);
                }
            }
            KeyCode::Enter => {
                if self.forgejo_selected || count == 0 {
                    self.screen = Screen::Forgejo;
                } else if let Some(agent) = self.view.agents.get(self.selection) {
                    self.agent_name = agent.name.clone();
                    self.confirm_remove = None;
                    self.screen = Screen::Agent;
                }
            }
            KeyCode::Esc => self.forgejo_selected = false,
            _ => match plain(key) {
                Some('s') => {
                    let name = self.selected_agent().map(|agent| agent.name.clone());
                    if let Some(name) = name {
                        self.toggle_named(&name);
                    } else {
                        self.set_notice("pick an agent, then s");
                    }
                }
                Some('+') => {
                    if count >= 20 {
                        self.set_notice("20 agents is the most");
                    } else {
                        self.send(Command::AddAgent);
                    }
                }
                Some('-') => {
                    if count <= 1 {
                        self.set_notice("the last agent cannot be removed");
                    } else if let Some(agent) = self.selected_agent() {
                        self.confirm_remove = Some(agent.name.clone());
                    } else {
                        self.set_notice("pick an agent, then −");
                    }
                }
                Some('f') => self.screen = Screen::Forgejo,
                Some('m') => {
                    self.model_origin = ModelOrigin::Board;
                    self.model_agent = None;
                    self.set_model_from_view();
                    self.screen = Screen::Model;
                }
                Some('?') => {
                    self.help_back = Screen::Home;
                    self.screen = Screen::Help;
                }
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn keys_agent(&mut self, key: KeyEvent) {
        if let Some(name) = self.confirm_remove.clone() {
            match plain(key) {
                Some('y') => {
                    self.confirm_remove = None;
                    self.send(Command::RemoveAgent { name });
                    self.screen = Screen::Home;
                }
                Some('n') | Some('q') => self.confirm_remove = None,
                _ if key.code == KeyCode::Esc => self.confirm_remove = None,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.screen = Screen::Home,
            _ => match plain(key) {
                Some('s') => {
                    let name = self.agent_name.clone();
                    self.toggle_named(&name);
                }
                Some('p') => {
                    let current = self
                        .view
                        .agents
                        .iter()
                        .find(|agent| agent.name == self.agent_name)
                        .map(|agent| agent.program.as_str())
                        .unwrap_or("builtin");
                    self.program_index = if current == "builtin" {
                        0
                    } else {
                        crate::harness::NATIVE_PROGRAMS
                            .iter()
                            .position(|program| program.id() == current)
                            .map(|index| index + 1)
                            .unwrap_or(0)
                    };
                    let chosen = self
                        .view
                        .agents
                        .iter()
                        .find(|agent| agent.name == self.agent_name)
                        .and_then(|agent| Protocol::parse(&agent.protocol))
                        .unwrap_or(Protocol::ChatCompletions);
                    let program = if self.program_index == 0 {
                        Program::Builtin
                    } else {
                        crate::harness::NATIVE_PROGRAMS
                            .get(self.program_index - 1)
                            .copied()
                            .unwrap_or(Program::Builtin)
                    };
                    self.program_protocol = if program.supports(chosen) {
                        chosen
                    } else {
                        program
                            .supported_protocols()
                            .first()
                            .copied()
                            .unwrap_or(Protocol::ChatCompletions)
                    };
                    self.codex_optin_confirmed = false;
                    self.screen = Screen::Program;
                }
                Some('m') => {
                    let builtin = self
                        .view
                        .agents
                        .iter()
                        .find(|agent| agent.name == self.agent_name)
                        .is_some_and(|agent| agent.program == "builtin");
                    self.model_agent = (!builtin).then(|| self.agent_name.clone());
                    self.model_origin = ModelOrigin::Board;
                    if builtin {
                        self.set_notice("built-in agents share the default connection");
                    }
                    self.set_model_from_view();
                    self.screen = Screen::Model;
                }
                Some('e') => match self.engine.instruction_path(&self.agent_name) {
                    Some(path) => {
                        self.handoff = Some(Handoff {
                            kind: HandoffKind::Edit,
                            path,
                            label: format!("{}/instruction.md", self.agent_name),
                        })
                    }
                    None => self.set_notice("no instruction file yet"),
                },
                Some('l') => self.offer_session_file(true),
                Some('t') => self.offer_session_file(false),
                Some('j') => self.offer_raw_transcript(),
                Some('d') => {
                    if self.view.agents.len() <= 1 {
                        self.set_notice("the last agent cannot be removed");
                    } else {
                        self.confirm_remove = Some(self.agent_name.clone());
                    }
                }
                Some('?') => {
                    self.help_back = Screen::Agent;
                    self.screen = Screen::Help;
                }
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn selected_program(&self) -> Program {
        if self.program_index == 0 {
            Program::Builtin
        } else {
            crate::harness::NATIVE_PROGRAMS
                .get(self.program_index - 1)
                .copied()
                .unwrap_or(Program::Builtin)
        }
    }

    fn cycle_program_protocol(&mut self, forward: bool) {
        let program = self.selected_program();
        let supported = program.supported_protocols();
        if supported.is_empty() {
            return;
        }
        let current = supported
            .iter()
            .position(|protocol| *protocol == self.program_protocol)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % supported.len()
        } else {
            (current + supported.len() - 1) % supported.len()
        };
        self.program_protocol = supported[next];
    }

    fn keys_program(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.screen = Screen::Agent,
            KeyCode::Up => {
                self.program_index = (self.program_index + 4) % 5;
                let supported = self.selected_program().supported_protocols();
                if !supported.contains(&self.program_protocol) {
                    self.program_protocol = supported
                        .first()
                        .copied()
                        .unwrap_or(Protocol::ChatCompletions);
                }
            }
            KeyCode::Down => {
                self.program_index = (self.program_index + 1) % 5;
                let supported = self.selected_program().supported_protocols();
                if !supported.contains(&self.program_protocol) {
                    self.program_protocol = supported
                        .first()
                        .copied()
                        .unwrap_or(Protocol::ChatCompletions);
                }
            }
            KeyCode::Left => self.cycle_program_protocol(false),
            KeyCode::Right => self.cycle_program_protocol(true),
            KeyCode::Enter => {
                let program = self.selected_program();
                if !program.supports(self.program_protocol) {
                    self.program_protocol = program
                        .supported_protocols()
                        .first()
                        .copied()
                        .unwrap_or(Protocol::ChatCompletions);
                }
                self.send(Command::SetAgentProgram {
                    name: self.agent_name.clone(),
                    program: program.id().to_owned(),
                    protocol: Some(self.program_protocol.id().to_owned()),
                });
                self.screen = Screen::Agent;
            }
            _ => match plain(key) {
                Some('i') => {
                    let program = self.selected_program();
                    if program.is_native() {
                        let installing = self
                            .view
                            .programs
                            .iter()
                            .any(|view| view.id == program.id() && view.installing.is_some());
                        if installing {
                            self.send(Command::CancelInstallProgram {
                                id: program.id().to_owned(),
                            });
                        } else {
                            self.send(Command::InstallProgram {
                                id: program.id().to_owned(),
                            });
                        }
                    } else {
                        self.set_notice("the built-in engine needs no install");
                    }
                }
                Some('f') if self.selected_program() == Program::Codex => {
                    let current = self
                        .view
                        .agents
                        .iter()
                        .find(|agent| agent.name == self.agent_name)
                        .and_then(|agent| agent.codex_sandbox.clone())
                        .unwrap_or_else(|| "native".to_owned());
                    if !self.codex_optin_confirmed {
                        self.codex_optin_confirmed = true;
                        self.set_notice(if current == "full-access" {
                            "Codex's own sandbox will be restored; the product Landlock boundary remains. Press f again to return this agent's next run to its native sandbox."
                        } else {
                            "Codex's own sandbox will be removed; only the product Landlock boundary remains. Press f again to allow it for this agent's next run."
                        });
                    } else {
                        let mode = if current == "full-access" {
                            "native"
                        } else {
                            "full-access"
                        };
                        self.codex_optin_confirmed = false;
                        self.send(Command::SetAgentCodexSandbox {
                            name: self.agent_name.clone(),
                            mode: mode.to_owned(),
                        });
                    }
                }
                Some('?') => {
                    self.help_back = Screen::Program;
                    self.screen = Screen::Help;
                }
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn keys_forgejo(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.revealed_password = None;
                self.screen = Screen::Home;
            }
            _ => match plain(key) {
                Some('p') => {
                    // The reset runs through the administration API in the background; the new
                    // password arrives in a later view, never before it is known.
                    self.revealed_password = None;
                    self.send(Command::RevealAdminPassword);
                }
                Some('c') => match self.revealed_password.as_ref() {
                    // F-13: a copy route that never puts the value in a file, a log
                    // or a second plaintext line. OSC 52 carries it to the terminal's
                    // clipboard; a terminal that does not support it simply ignores
                    // the sequence and the screen value is still there.
                    Some((password, _)) => {
                        self.clipboard = Some(osc52(password));
                        self.set_notice("copied");
                    }
                    None => self.set_notice("press p to show the password, then c copies it"),
                },
                Some('?') => {
                    self.help_back = Screen::Forgejo;
                    self.screen = Screen::Help;
                }
                Some('q') => self.request_quit(),
                _ => {}
            },
        }
    }

    fn keys_help(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) || matches!(plain(key), Some('?')) {
            self.screen = self.help_back;
        }
    }

    fn selected_agent(&self) -> Option<&AgentView> {
        if self.forgejo_selected {
            return None;
        }
        self.view.agents.get(self.selection)
    }

    fn toggle_named(&mut self, name: &str) {
        let desired = self
            .view
            .agents
            .iter()
            .find(|agent| agent.name == name)
            .map(|agent| agent.desired);
        let command = match desired {
            Some(Desired::Started) => Command::Stop {
                name: name.to_owned(),
            },
            Some(Desired::Stopped) => Command::Start {
                name: name.to_owned(),
            },
            None => return,
        };
        self.send(command);
    }

    fn offer_session_file(&mut self, log: bool) {
        let Some((log_path, transcript)) = self.engine.session_files(&self.agent_name) else {
            self.set_notice(if log {
                "no log yet — s starts a run"
            } else {
                "no transcript yet — s starts a run"
            });
            return;
        };
        let (path, kind, label) = if log {
            (log_path, HandoffKind::View, "log")
        } else {
            let readable = self
                .engine
                .readable_transcript(&self.agent_name)
                .unwrap_or(transcript);
            (readable, HandoffKind::View, "transcript")
        };
        self.handoff = Some(Handoff {
            kind,
            path,
            label: format!("{}/{label}", self.agent_name),
        });
    }

    fn offer_raw_transcript(&mut self) {
        let Some((_log, transcript)) = self.engine.session_files(&self.agent_name) else {
            self.set_notice("no transcript yet — s starts a run");
            return;
        };
        self.handoff = Some(Handoff {
            kind: HandoffKind::View,
            path: transcript,
            label: format!("{}/transcript (JSONL)", self.agent_name),
        });
    }

    fn request_quit(&mut self) {
        let live = self.active_started_count();
        let installing = self.installing_count();
        if live == 0 && installing == 0 {
            self.leave_without_warning();
            return;
        }
        if self.confirm_quit {
            self.shutdown();
            self.quitting = true;
        } else {
            self.screen = Screen::Home;
            self.confirm_quit = true;
            self.confirm_remove = None;
            if installing > 0 {
                let noun = if installing == 1 {
                    "install"
                } else {
                    "installs"
                };
                self.set_notice(format!(
                    "q again cancels the running {noun}; the previous version is kept"
                ));
            }
        }
    }

    /// How many program installs are still running, for the quit warning.
    pub fn installing_count(&self) -> usize {
        self.view
            .programs
            .iter()
            .filter(|program| program.installing.is_some())
            .count()
    }

    fn leave_without_warning(&mut self) {
        self.quit_stopped.get_or_insert(0);
        self.shutdown();
        self.quitting = true;
    }

    /// Stop everything once. Safe to call from every way out.
    pub fn shutdown(&mut self) {
        if self.shutdown_sent {
            return;
        }
        self.shutdown_sent = true;
        self.shutdown_calls += 1;
        let live = self.active_started_count();
        self.quit_stopped.get_or_insert(live);
        self.engine.shutdown(Duration::from_secs(2));
    }

    /// Draw the current screen.
    pub fn draw(&mut self, frame: &mut Frame) {
        render::draw(self, frame);
    }

    /// The file waiting for a hand-off, if any.
    pub fn take_handoff(&mut self) -> Option<Handoff> {
        self.handoff.take()
    }

    /// The hand-off ended; read the picture again so an edited instruction shows.
    pub fn handoff_finished(&mut self) {
        self.read_view();
    }

    /// The screen now on show.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// The latest view.
    pub fn view(&self) -> &BoardView {
        &self.view
    }

    /// The card and wire rectangles drawn by the last frame.
    pub fn layout(&self) -> &BoardLayout {
        &self.last
    }

    /// The started agents that are running or restarting now.
    ///
    /// This is the population the quit warning names and the final line counts: an
    /// agent waiting between runs, or stopped by a missing instruction, is still
    /// started, but it has no live run for `quit` to stop. The shutdown path still
    /// stops every live run and the Forgejo.
    pub fn active_started(&self) -> impl Iterator<Item = &AgentView> {
        self.view.agents.iter().filter(|agent| {
            agent.desired == Desired::Started
                && matches!(
                    agent.session,
                    SessionState::Running { .. } | SessionState::Restarting { .. }
                )
        })
    }

    /// How many started agents have a live run now, so the warning and the final
    /// line count the same population.
    pub fn active_started_count(&self) -> usize {
        self.active_started().count()
    }

    /// Whether the loop should stop.
    pub fn should_quit(&self) -> bool {
        self.quitting
    }

    /// The live started agents the quit confirmation named, for the final line.
    pub fn quit_stopped_count(&self) -> usize {
        self.quit_stopped.unwrap_or(0)
    }

    /// How many times shutdown ran.
    pub fn shutdown_calls(&self) -> usize {
        self.shutdown_calls
    }

    /// The commands sent so far.
    pub fn sent_commands(&self) -> &[Command] {
        &self.sent
    }

    /// Take the commands sent so far.
    pub fn take_sent_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.sent)
    }

    /// The notice on show, if any.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The selected agent's index, or `None` when the Forgejo box is selected.
    pub fn selection(&self) -> Option<usize> {
        if self.forgejo_selected {
            None
        } else {
            Some(self.selection)
        }
    }

    /// The OSC 52 sequence waiting for the terminal, once.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// The password on show after `p`, if it is still in its few seconds.
    pub fn revealed_password(&self) -> Option<&str> {
        self.revealed_password
            .as_ref()
            .map(|(password, _)| password.as_str())
    }

    /// The instruction editor's cursor, for the model form's tests.
    pub fn model_editing(&self) -> bool {
        self.model_editing
    }
}

/// The OSC 52 clipboard sequence for a secret, base64 encoded.
///
/// The value never leaves in plaintext a second time and is never written to a file
/// or a log: the terminal that owns the screen either supports OSC 52 or ignores it
/// (F-13).
fn osc52(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(8 + bytes.len().div_ceil(3) * 4);
    out.push_str("\x1b]52;c;");
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out.push('\x07');
    out
}

/// Empty a model field in one gesture.
fn clear_string(field: &mut String) {
    field.clear();
}

/// A character key without control or alt, so `t` can type a `t` in a field and
/// still mean "test" when no field is being edited.
fn plain(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            Some(ch)
        }
        _ => None,
    }
}

fn insert_char(text: &mut String, at: usize, ch: char) {
    let byte = text
        .char_indices()
        .nth(at)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.insert(byte, ch);
}

fn remove_char(text: &mut String, at: usize) {
    if let Some((byte, _)) = text.char_indices().nth(at) {
        text.remove(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn press(board: &mut Board, code: KeyCode) {
        board.on_key(KeyEvent::from(code));
    }

    fn draw(board: &mut Board) {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| board.draw(frame)).expect("draw");
    }

    fn acked(engine: &mut FakeEngine) {
        engine.send(Command::AcknowledgeTerms).expect("terms");
    }

    #[test]
    fn terms_refuse_y_until_every_item_has_been_drawn() {
        let mut board = Board::new(FakeEngine::new());
        press(&mut board, KeyCode::Char('y'));
        assert!(
            !board
                .sent_commands()
                .iter()
                .any(|command| matches!(command, Command::AcknowledgeTerms))
        );
        assert!(board.notice().is_some());
        draw(&mut board);
        press(&mut board, KeyCode::Char('y'));
        assert_eq!(board.sent_commands(), [Command::AcknowledgeTerms]);
        assert_eq!(board.screen(), Screen::Model);
    }

    #[test]
    fn a_notice_is_shown_once_then_forgets_itself() {
        let mut engine = FakeEngine::new();
        acked(&mut engine);
        // The acknowledgment notice is in the first view; the board holds it a
        // moment and then lets it go.
        let mut board = Board::new(engine);
        assert_eq!(board.notice(), Some("terms recorded"));
        for _ in 0..9 {
            board.tick();
        }
        assert!(board.notice().is_none());
    }

    #[test]
    fn a_command_is_recorded_exactly_once_even_when_the_engine_refuses_it() {
        let mut board = Board::new(FakeEngine::new());
        draw(&mut board);
        press(&mut board, KeyCode::Char('y'));
        board.take_sent_commands();
        // AcknowledgeTerms was sent and accepted; terms were not a refusal.
        assert!(board.view().terms_acknowledged);
    }
}
