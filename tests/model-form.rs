//! The model form's focus, its one Tab behaviour, and the empty-key error.
//!
//! These are the seams the walk could see without a provider: nothing is sent for an
//! empty key, so `the API key was refused` can never be said about a request that
//! never left the machine, and the same Tab moves in view mode and in edit mode.

use gewu_multiverse::board::{Board, FakeEngine, Screen};
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
}

fn type_text(board: &mut Board, text: &str) {
    for ch in text.chars() {
        press(board, KeyCode::Char(ch));
    }
}

fn frame_text(board: &mut Board, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    let buffer: Buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

/// A configured model without a key: the provider is known, the key is not there.
fn model_board_without_a_key() -> Board {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: None,
        })
        .expect("endpoint");
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Picker);
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Model);
    board
}

#[test]
fn an_empty_key_is_its_own_error_before_any_request() {
    let mut board = model_board_without_a_key();
    press(&mut board, KeyCode::Char('t'));
    let notice = board.notice().unwrap_or_default();
    assert!(
        notice.contains("the API key is empty"),
        "the notice is {notice:?}"
    );
    assert!(
        board.sent_commands().is_empty(),
        "a request was sent without a key: {:?}",
        board.sent_commands()
    );
    let frame = frame_text(&mut board, 80, 24);
    assert!(!frame.contains("your model is saved"), "{frame}");
    assert!(!frame.contains("the API key was refused"), "{frame}");
    assert!(frame.contains("the API key is empty"), "{frame}");
}

#[test]
fn one_tab_moves_between_fields_while_editing() {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Model);
    // Address.
    press(&mut board, KeyCode::Char('e'));
    type_text(&mut board, "https://model.example/v1/chat/completions");
    press(&mut board, KeyCode::Tab);
    // The Tab kept the edit and moved to `model`.
    type_text(&mut board, "demo-model");
    press(&mut board, KeyCode::Tab);
    type_text(&mut board, "demo-key");
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('t'));
    let set = board
        .sent_commands()
        .iter()
        .find_map(|command| match command {
            Command::SetEndpoint { url, model, key } => {
                Some((url.clone(), model.clone(), key.clone()))
            }
            _ => None,
        })
        .expect("the endpoint is set before the test");
    assert_eq!(set.0, "https://model.example/v1/chat/completions");
    assert_eq!(set.1, "demo-model");
    assert_eq!(set.2.as_deref(), Some("demo-key"));
    assert!(board.notice().is_none(), "{:?}", board.notice());
}

#[test]
fn the_focused_field_is_visible_in_view_mode() {
    let mut board = model_board_without_a_key();
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("▸ API address"), "{frame}");
    press(&mut board, KeyCode::Down);
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("▸ model"), "{frame}");
    assert!(!frame.contains("▸ API address"), "{frame}");
}

#[test]
fn the_same_verb_opens_and_saves_the_model() {
    // First run: from the picker back to the model.
    let mut first = model_board_without_a_key();
    let frame = frame_text(&mut first, 80, 24);
    assert!(frame.contains("⏎ next"), "{frame}");
    assert!(!frame.contains("⏎ save"), "{frame}");

    // Later: `m` from the board.
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    engine.send(Command::StartForgejo).expect("Forgejo");
    for _ in 0..130 {
        engine.tick();
    }
    let _ = engine.board();
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Home);
    press(&mut board, KeyCode::Char('m'));
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("⏎ next"), "{frame}");
    assert!(!frame.contains("⏎ save"), "{frame}");
}

/// A model screen whose provider answered a realistic 404 on a completions path
/// without `/v1`, with the sentence the string builder really produces.
struct NotFoundEngine(FakeEngine);

impl EngineApi for NotFoundEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.endpoint.test = gewu_multiverse::engine_api::EndpointTest::Failed {
            reason: gewu_multiverse::model::failure_words(
                &gewu_multiverse::model::ModelError::RouteNotFound {
                    endpoint: "http://127.0.0.1:18192/chat/completions".to_owned(),
                    status: 404,
                    message: "Invalid URL (POST /chat/completions).".to_owned(),
                },
                "demo-model",
            ),
        };
        view
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        self.0.send(command)
    }

    fn tick(&mut self) {
        self.0.tick()
    }

    fn instruction_path(&self, agent: &str) -> Option<std::path::PathBuf> {
        self.0.instruction_path(agent)
    }

    fn session_files(&self, agent: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
        self.0.session_files(agent)
    }

    fn shutdown(&mut self, grace: std::time::Duration) {
        self.0.shutdown(grace)
    }
}

#[test]
fn the_v1_repair_is_visible_at_eighty_by_twenty_four() {
    // Review B1: the persona's own 80x24 screen showed `— add…` or no `/v1` at all
    // because the repair trailed the provider's clause in one ellipsized row.
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "http://127.0.0.1:18192/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("sk-test-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    engine.send(Command::StartForgejo).expect("Forgejo");
    for _ in 0..130 {
        engine.tick();
    }
    let _ = engine.board();

    let mut board = Board::new(NotFoundEngine(engine));
    assert_eq!(board.screen(), Screen::Home);
    press(&mut board, KeyCode::Char('m'));
    assert_eq!(board.screen(), Screen::Model);
    let frame = frame_text(&mut board, 80, 24);
    let repair = frame
        .find("add /v1")
        .unwrap_or_else(|| panic!("no /v1 repair at 80x24:\n{frame}"));
    let status = frame
        .find("HTTP 404")
        .unwrap_or_else(|| panic!("no provider status at 80x24:\n{frame}"));
    assert!(repair < status, "the repair is not first:\n{frame}");
    assert!(
        frame.contains("Invalid URL"),
        "the provider's own clause is hidden:\n{frame}"
    );
}
