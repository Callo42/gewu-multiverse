//! A terminal paste is one user action in the model form.
//!
//! The form has three one-line text fields (address, model, key), in the first-run
//! form and in a native agent's form, which is the only text input the TUI has. A
//! paste fills the focused field whole even when it is not in edit mode, and its
//! characters are text: `e`, `t` and `q` inside a pasted address must not start an
//! edit, run a test or ask to leave.

use gewu_multiverse::board::{Board, FakeEngine, Screen};
use gewu_multiverse::engine_api::{Command, EngineApi};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

const ADDRESS: &str = "http://127.0.0.1:20001/v1/chat/completions";

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
}

fn model_board() -> Board {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    let board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Model);
    board
}

fn frame_text(board: &mut Board, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn a_paste_fills_each_model_field_whole_without_an_edit_key() {
    let mut board = model_board();
    board.on_paste(ADDRESS);
    assert!(
        !board.model_editing(),
        "a paste must not change what the next key does"
    );
    let frame = frame_text(&mut board, 100, 30);
    assert!(
        frame.contains(ADDRESS),
        "the address is not whole:\n{frame}"
    );

    press(&mut board, KeyCode::Down);
    board.on_paste("demo-model");
    let frame = frame_text(&mut board, 100, 30);
    assert!(
        frame.contains("demo-model"),
        "the model is not whole:\n{frame}"
    );

    press(&mut board, KeyCode::Down);
    board.on_paste("sk-pasted-key");
    let frame = frame_text(&mut board, 100, 30);
    assert!(
        !frame.contains("sk-pasted-key"),
        "the pasted key is shown in the clear:\n{frame}"
    );
    assert!(
        frame.contains("••••••••"),
        "the key field is empty:\n{frame}"
    );
}

#[test]
fn enter_after_a_paste_still_advances_a_field_being_edited() {
    let mut board = model_board();
    press(&mut board, KeyCode::Char('e'));
    board.on_paste(ADDRESS);
    press(&mut board, KeyCode::Enter);
    assert!(!board.model_editing(), "Enter did not commit the field");
    let frame = frame_text(&mut board, 100, 30);
    assert!(frame.contains(ADDRESS), "the address was lost:\n{frame}");
    assert!(frame.contains("▸ model"), "Enter did not advance:\n{frame}");
}

#[test]
fn paste_letters_are_text_not_the_forms_keys() {
    let mut board = model_board();
    // Every one of these letters is a binding on this screen: `e` edits, `t`
    // tests and `q` asks to leave. None may act while the line is pasted.
    board.on_paste("e t q");
    assert_eq!(
        board.screen(),
        Screen::Model,
        "the paste changed the screen"
    );
    assert!(!board.should_quit(), "a pasted q asked to leave");
    assert!(
        board.sent_commands().is_empty(),
        "a pasted t ran a command: {:?}",
        board.sent_commands()
    );
    let frame = frame_text(&mut board, 100, 30);
    assert!(frame.contains("e t q"), "the paste was not text:\n{frame}");
}

#[test]
fn a_paste_keeps_the_first_line_of_a_one_line_field() {
    let mut board = model_board();
    board.on_paste(&format!("{ADDRESS}\nnot part of the address"));
    let frame = frame_text(&mut board, 100, 30);
    assert!(
        frame.contains(ADDRESS),
        "the first line is not whole:\n{frame}"
    );
    assert!(
        !frame.contains("not part of the address"),
        "a second line joined the one-line field:\n{frame}"
    );
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("first line")),
        "the unused lines were dropped in silence: {:?}",
        board.notice()
    );
}

#[test]
fn pasting_the_same_value_twice_replaces_instead_of_doubling() {
    let mut board = model_board();
    board.on_paste(ADDRESS);
    board.on_paste(ADDRESS);
    let frame = frame_text(&mut board, 100, 30);
    assert_eq!(
        frame.matches(ADDRESS).count(),
        1,
        "the second paste doubled the field:\n{frame}"
    );
}

#[test]
fn a_pasted_key_is_saved_whole_and_stays_masked() {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: ADDRESS.to_owned(),
            model: "demo-model".to_owned(),
            key: None,
        })
        .expect("endpoint");
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Picker);
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Model);
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Down);
    board.on_paste("sk-pasted-key");
    let frame = frame_text(&mut board, 100, 30);
    assert!(
        !frame.contains("sk-pasted-key"),
        "the key is in the clear:\n{frame}"
    );
    assert!(
        frame.contains("••••••••"),
        "the key field is empty:\n{frame}"
    );
    press(&mut board, KeyCode::Enter);
    let key = board
        .sent_commands()
        .iter()
        .find_map(|command| match command {
            Command::SetEndpoint { key, .. } => Some(key.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the key was not saved; the notice is {:?}", board.notice()));
    assert_eq!(key.as_deref(), Some("sk-pasted-key"));
}

#[test]
fn a_paste_outside_the_model_form_changes_nothing() {
    let mut board = Board::new(FakeEngine::new());
    assert_eq!(board.screen(), Screen::Terms);
    board.on_paste(ADDRESS);
    assert_eq!(board.screen(), Screen::Terms);
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("no text field")),
        "a paste with no field was dropped in silence: {:?}",
        board.notice()
    );
}
