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

use crate::engine_api::{AgentView, BoardView, Command, Desired, EngineApi, Provisioning};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::Duration;

pub use fake::FakeEngine;
pub use render::BoardLayout;
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
    /// The one Forgejo.
    Forgejo,
    /// The keys.
    Help,
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
    model_origin: ModelOrigin,
    agent_count: usize,
    build_ready_ticks: u8,
    agent_name: String,
    confirm_remove: Option<String>,
    confirm_quit: bool,
    notice: Option<String>,
    notice_ticks: u8,
    revealed_password: Option<(String, u8)>,
    sent: Vec<Command>,
    handoff: Option<Handoff>,
    quitting: bool,
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
        let screen = if !view.terms_acknowledged {
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
            model_origin: ModelOrigin::FirstRun,
            agent_count: 3,
            build_ready_ticks: 0,
            agent_name: String::new(),
            confirm_remove: None,
            confirm_quit: false,
            notice: None,
            notice_ticks: 0,
            revealed_password: None,
            sent: Vec::new(),
            handoff: None,
            quitting: false,
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
            self.revealed_password = Some((password, 8));
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
        self.model_fields[0] = self.view.endpoint.url.clone();
        self.model_fields[1] = self.view.endpoint.model.clone();
        self.model_fields[2].clear();
        self.model_focus = 0;
        self.model_editing = false;
        self.model_cursor = self.model_fields[0].chars().count();
    }

    fn endpoint_command(&self) -> Command {
        let key = if self.model_fields[2].is_empty() {
            None
        } else {
            Some(self.model_fields[2].clone())
        };
        Command::SetEndpoint {
            url: self.model_fields[0].trim().to_owned(),
            model: self.model_fields[1].trim().to_owned(),
            key,
        }
    }

    fn endpoint_fields_filled(&self) -> bool {
        !self.model_fields[0].trim().is_empty() && !self.model_fields[1].trim().is_empty()
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
            Screen::Forgejo => self.keys_forgejo(key),
            Screen::Help => self.keys_help(key),
        }
    }

    fn keys_terms(&mut self, key: KeyEvent) {
        match plain(key) {
            Some('y') => {
                if self.terms_seen.iter().all(|seen| *seen) {
                    self.send(Command::AcknowledgeTerms);
                    self.screen = Screen::Model;
                    self.model_origin = ModelOrigin::FirstRun;
                    self.set_model_from_view();
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
        if self.model_editing {
            self.edit_field(key);
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.model_focus = (self.model_focus + 2) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
            }
            KeyCode::Down | KeyCode::Tab => {
                self.model_focus = (self.model_focus + 1) % 3;
                self.model_cursor = self.model_fields[self.model_focus].chars().count();
            }
            KeyCode::Enter => {
                if !self.endpoint_fields_filled() {
                    self.set_notice("API address and model needed, then ⏎");
                    return;
                }
                self.send(self.endpoint_command());
                self.screen = match self.model_origin {
                    ModelOrigin::FirstRun => Screen::Picker,
                    ModelOrigin::Board => Screen::Home,
                };
            }
            KeyCode::Esc => {
                self.screen = match self.model_origin {
                    ModelOrigin::FirstRun => Screen::Terms,
                    ModelOrigin::Board => Screen::Home,
                };
            }
            _ => match plain(key) {
                Some('t') => {
                    if !self.endpoint_fields_filled() {
                        self.set_notice("API address and model needed, then t");
                    } else {
                        self.send(self.endpoint_command());
                        self.send(Command::TestEndpoint);
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

    fn edit_field(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => self.model_editing = false,
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
                }
            }
            KeyCode::Delete => {
                remove_char(&mut self.model_fields[self.model_focus], self.model_cursor);
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
                        self.set_notice("one agent is the least");
                    } else if let Some(agent) = self.selected_agent() {
                        self.confirm_remove = Some(agent.name.clone());
                    } else {
                        self.set_notice("pick an agent, then −");
                    }
                }
                Some('f') => self.screen = Screen::Forgejo,
                Some('m') => {
                    self.model_origin = ModelOrigin::Board;
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
                Some('d') => {
                    if self.view.agents.len() <= 1 {
                        self.set_notice("one agent is the least");
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

    fn keys_forgejo(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.revealed_password = None;
                self.screen = Screen::Home;
            }
            _ => match plain(key) {
                Some('p') => {
                    self.send(Command::RevealAdminPassword);
                    if self.revealed_password.is_none() {
                        self.set_notice("password not available");
                    }
                }
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
            (transcript, HandoffKind::View, "transcript")
        };
        self.handoff = Some(Handoff {
            kind,
            path,
            label: format!("{}/{label}", self.agent_name),
        });
    }

    fn request_quit(&mut self) {
        let started = self
            .view
            .agents
            .iter()
            .any(|agent| agent.desired == Desired::Started);
        if !started {
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
        }
    }

    fn leave_without_warning(&mut self) {
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

    /// Whether the loop should stop.
    pub fn should_quit(&self) -> bool {
        self.quitting
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
