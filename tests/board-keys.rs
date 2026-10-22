//! Key tests for the board: every action sends exactly the command it promises.

use std::fs;

use gewu_multiverse::board::{Board, FakeEngine, HandoffKind, Screen};
use gewu_multiverse::engine_api::{Command, EndpointTest, EngineApi};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn endpoint_engine(count: usize) -> FakeEngine {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    let first = count.min(12);
    engine
        .send(Command::CreateAgents { count: first })
        .expect("agents");
    for _ in first..count {
        engine.send(Command::AddAgent).expect("one more agent");
    }
    engine.send(Command::StartForgejo).expect("Forgejo");
    for _ in 0..(120 + count * 2) {
        engine.tick();
    }
    let _ = engine.board();
    engine
}

fn started_engine(count: usize) -> FakeEngine {
    let mut engine = endpoint_engine(count);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    fs::write(path, "fix the flaky login test\n").expect("the instruction");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    engine.tick();
    let _ = engine.board();
    engine
}

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
}

fn type_text(board: &mut Board, text: &str) {
    for ch in text.chars() {
        press(board, KeyCode::Char(ch));
    }
}

fn draw(board: &mut Board) -> Buffer {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    terminal.backend().buffer().clone()
}

fn frame_text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

fn first_run_model() -> Board {
    let mut board = Board::new(FakeEngine::new());
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Char('y'));
    assert_eq!(board.screen(), Screen::Model);
    board.take_sent_commands();
    board
}

fn fill_model(board: &mut Board, url: &str, model: &str, key: &str) {
    press(board, KeyCode::Char('e'));
    type_text(board, url);
    press(board, KeyCode::Enter);
    press(board, KeyCode::Char('e'));
    type_text(board, model);
    press(board, KeyCode::Enter);
    press(board, KeyCode::Char('e'));
    type_text(board, key);
    press(board, KeyCode::Enter);
}

#[test]
fn the_terms_send_acknowledge_only_after_every_item_was_drawn() {
    let mut board = Board::new(FakeEngine::new());
    press(&mut board, KeyCode::Char('y'));
    assert!(board.sent_commands().is_empty());
    assert!(board.notice().unwrap().contains("read every item"));
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Char('y'));
    assert_eq!(board.sent_commands(), [Command::AcknowledgeTerms]);
    assert_eq!(board.screen(), Screen::Model);
}

#[test]
fn n_on_the_terms_leaves_without_sending_a_command() {
    let mut board = Board::new(FakeEngine::new());
    press(&mut board, KeyCode::Char('n'));
    assert!(board.should_quit());
    assert_eq!(board.shutdown_calls(), 1);
    assert!(board.sent_commands().is_empty());
}

#[test]
fn the_model_tests_and_saves_with_the_expected_commands() {
    let mut board = first_run_model();
    fill_model(
        &mut board,
        "https://model.example/v1/chat/completions",
        "demo-model",
        "demo-key",
    );
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('t'));
    assert_eq!(
        board.sent_commands(),
        [
            Command::SetEndpoint {
                url: "https://model.example/v1/chat/completions".to_owned(),
                model: "demo-model".to_owned(),
                key: Some("demo-key".to_owned()),
            },
            Command::TestEndpoint,
        ]
    );
    board.tick();
    assert!(matches!(
        board.view().endpoint.test,
        EndpointTest::Passed { .. }
    ));

    board.take_sent_commands();
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Picker);
    assert_eq!(
        board.sent_commands(),
        [Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        }]
    );
}

#[test]
fn the_model_refuses_to_go_on_with_an_empty_url_or_model() {
    let mut board = first_run_model();
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Model);
    assert!(board.sent_commands().is_empty());
    assert!(board.notice().unwrap().contains("API address and model"));
}

#[test]
fn the_picker_creates_the_chosen_count_and_starts_forgejo() {
    let mut board = first_run_model();
    fill_model(
        &mut board,
        "https://model.example/v1/chat/completions",
        "demo-model",
        "demo-key",
    );
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Picker);
    board.take_sent_commands();
    press(&mut board, KeyCode::Right);
    press(&mut board, KeyCode::Right);
    press(&mut board, KeyCode::Left);
    press(&mut board, KeyCode::Char('+'));
    press(&mut board, KeyCode::Char('-'));
    press(&mut board, KeyCode::Enter);
    assert_eq!(
        board.sent_commands(),
        [Command::CreateAgents { count: 4 }, Command::StartForgejo,]
    );
    assert_eq!(board.screen(), Screen::Build);
}

#[test]
fn the_board_starts_and_stops_the_selected_agent() {
    let mut board = Board::new(endpoint_engine(3));
    assert_eq!(board.screen(), Screen::Home);
    press(&mut board, KeyCode::Char('s'));
    assert_eq!(
        board.sent_commands(),
        [Command::Start {
            name: "agent-1".to_owned()
        }]
    );
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('s'));
    assert_eq!(
        board.sent_commands(),
        [Command::Stop {
            name: "agent-1".to_owned()
        }]
    );
}

#[test]
fn arrows_select_agents_and_the_forgejo_box() {
    let mut board = Board::new(endpoint_engine(7));
    let _ = draw(&mut board);
    assert_eq!(board.selection(), Some(0));
    press(&mut board, KeyCode::Right);
    assert_eq!(board.selection(), Some(1));
    press(&mut board, KeyCode::Down);
    assert_eq!(board.selection(), Some(5));
    press(&mut board, KeyCode::Up);
    assert_eq!(board.selection(), Some(1));
    press(&mut board, KeyCode::Up);
    assert_eq!(board.selection(), None, "the Forgejo box is selected");
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Forgejo);
}

#[test]
fn plus_adds_and_minus_asks_once_before_removing() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('+'));
    assert_eq!(board.sent_commands(), [Command::AddAgent]);
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('-'));
    assert!(board.sent_commands().is_empty(), "the remove waits for y");
    let frame = frame_text(&draw(&mut board));
    assert!(
        frame.contains("remove agent-1?")
            && frame.contains("history kept")
            && frame.contains("y remove   n keep"),
        "{frame}"
    );
    press(&mut board, KeyCode::Char('y'));
    assert_eq!(
        board.sent_commands(),
        [Command::RemoveAgent {
            name: "agent-1".to_owned()
        }]
    );
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn plus_at_the_twenty_agent_cap_sends_nothing_and_says_so() {
    let mut board = Board::new(endpoint_engine(20));
    press(&mut board, KeyCode::Char('+'));
    assert!(board.sent_commands().is_empty());
    assert!(board.notice().unwrap().contains("20 agents"));
}

#[test]
fn the_agent_screen_offers_the_editor_and_both_pagers() {
    let engine = started_engine(1);
    let log = engine.session_files("agent-1").expect("session files");
    assert!(log.0.is_file() && log.1.is_file());
    let mut board = Board::new(engine);
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);

    press(&mut board, KeyCode::Char('e'));
    let handoff = board.take_handoff().expect("an editor hand-off");
    assert_eq!(handoff.kind, HandoffKind::Edit);
    assert!(handoff.path.ends_with("instruction.md"));

    press(&mut board, KeyCode::Char('l'));
    let handoff = board.take_handoff().expect("a log hand-off");
    assert_eq!(handoff.kind, HandoffKind::View);
    assert!(handoff.path.ends_with("run.log"));

    press(&mut board, KeyCode::Char('t'));
    let handoff = board.take_handoff().expect("a transcript hand-off");
    assert_eq!(handoff.kind, HandoffKind::View);
    assert!(handoff.path.ends_with("transcript.jsonl"));
}

#[test]
fn the_agent_screen_starts_stops_and_removes_after_one_confirmation() {
    let mut board = Board::new(started_engine(2));
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Enter);
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('s'));
    assert_eq!(
        board.sent_commands(),
        [Command::Stop {
            name: "agent-1".to_owned()
        }]
    );
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('d'));
    assert!(board.sent_commands().is_empty());
    press(&mut board, KeyCode::Char('n'));
    assert!(board.sent_commands().is_empty());
    press(&mut board, KeyCode::Char('d'));
    press(&mut board, KeyCode::Char('y'));
    assert_eq!(
        board.sent_commands(),
        [Command::RemoveAgent {
            name: "agent-1".to_owned()
        }]
    );
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn forgejo_reveals_the_password_once_and_never_keeps_it() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('f'));
    assert_eq!(board.screen(), Screen::Forgejo);
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('p'));
    assert_eq!(board.sent_commands(), [Command::RevealAdminPassword]);
    let frame = frame_text(&draw(&mut board));
    assert!(frame.contains("demo-admin-pw"), "{frame}");
    for _ in 0..45 {
        board.tick();
    }
    let frame = frame_text(&draw(&mut board));
    assert!(!frame.contains("demo-admin-pw"), "{frame}");
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn m_opens_the_model_without_a_command_and_t_tests_it() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('m'));
    assert_eq!(board.screen(), Screen::Model);
    assert!(board.sent_commands().is_empty());
    press(&mut board, KeyCode::Char('t'));
    assert!(matches!(
        board.sent_commands(),
        [Command::SetEndpoint { .. }, Command::TestEndpoint]
    ));
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn help_opens_and_closes_without_a_command() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('?'));
    assert_eq!(board.screen(), Screen::Help);
    assert!(board.sent_commands().is_empty());
    press(&mut board, KeyCode::Char('?'));
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn q_quits_at_once_when_every_agent_is_stopped() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('q'));
    assert!(board.should_quit());
    assert_eq!(board.shutdown_calls(), 1);
    assert!(board.sent_commands().is_empty());
}

#[test]
fn q_asks_once_when_an_agent_is_started_and_the_second_q_stops_everything() {
    let mut board = Board::new(started_engine(3));
    press(&mut board, KeyCode::Char('q'));
    assert!(!board.should_quit());
    assert_eq!(board.shutdown_calls(), 0);
    let frame = frame_text(&draw(&mut board));
    let flat = frame
        .replace(['│', '╭', '╮', '╰', '╯', '─'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        flat.contains("quit stops agent-1 and Forgejo; they start again on reopen — q again quits"),
        "{flat}"
    );
    assert!(board.sent_commands().is_empty());
    press(&mut board, KeyCode::Char('q'));
    assert!(board.should_quit());
    assert_eq!(board.shutdown_calls(), 1);
}

#[test]
fn the_notice_is_shown_once_and_then_goes() {
    let mut board = Board::new(endpoint_engine(1));
    press(&mut board, KeyCode::Char('+'));
    let frame = frame_text(&draw(&mut board));
    assert!(frame.contains("agent-2 added"), "{frame}");
    for _ in 0..9 {
        board.tick();
    }
    assert!(board.notice().is_none());
}

#[test]
fn the_last_agent_is_not_removed() {
    let mut board = Board::new(endpoint_engine(1));
    press(&mut board, KeyCode::Char('-'));
    assert!(board.sent_commands().is_empty());
    assert!(
        board
            .notice()
            .unwrap()
            .contains("the last agent cannot be removed")
    );
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('d'));
    assert!(board.sent_commands().is_empty());
    assert_eq!(board.screen(), Screen::Agent);
}

#[test]
fn escape_on_the_picker_returns_to_the_model_without_a_command() {
    let mut board = first_run_model();
    fill_model(
        &mut board,
        "https://model.example/v1/chat/completions",
        "demo-model",
        "demo-key",
    );
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Picker);
    board.take_sent_commands();
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Model);
    assert!(board.sent_commands().is_empty());
}

#[test]
fn the_picker_stops_at_one_and_twelve() {
    let mut board = picker_board_for_keys();
    for _ in 0..15 {
        press(&mut board, KeyCode::Right);
    }
    assert_eq!(frame_text(&draw(&mut board)).matches("−  12  +").count(), 1);
    for _ in 0..15 {
        press(&mut board, KeyCode::Left);
    }
    assert_eq!(frame_text(&draw(&mut board)).matches("−  1  +").count(), 1);
    board.take_sent_commands();
    press(&mut board, KeyCode::Enter);
    assert_eq!(
        board.sent_commands(),
        [Command::CreateAgents { count: 1 }, Command::StartForgejo]
    );
}

#[test]
fn the_agent_screen_escapes_and_opens_help_without_a_command() {
    let mut board = Board::new(started_engine(2));
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('?'));
    assert_eq!(board.screen(), Screen::Help);
    press(&mut board, KeyCode::Char('?'));
    assert_eq!(board.screen(), Screen::Agent);
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Home);
    assert!(board.sent_commands().is_empty());
}

#[test]
fn the_forgejo_screen_escapes_and_opens_help_without_a_command() {
    let mut board = Board::new(endpoint_engine(2));
    press(&mut board, KeyCode::Char('f'));
    assert_eq!(board.screen(), Screen::Forgejo);
    board.take_sent_commands();
    press(&mut board, KeyCode::Char('?'));
    assert_eq!(board.screen(), Screen::Help);
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Forgejo);
    press(&mut board, KeyCode::Esc);
    assert_eq!(board.screen(), Screen::Home);
    assert!(board.sent_commands().is_empty());
}

#[test]
fn the_last_agent_cannot_be_removed_from_the_board_or_its_panel() {
    let mut board = Board::new(endpoint_engine(1));
    press(&mut board, KeyCode::Char('-'));
    assert!(board.sent_commands().is_empty());
    assert!(
        board
            .notice()
            .unwrap()
            .contains("the last agent cannot be removed")
    );
    let _ = draw(&mut board);
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('d'));
    assert!(board.sent_commands().is_empty());
    assert_eq!(board.screen(), Screen::Agent);
}

fn picker_board_for_keys() -> Board {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    Board::new(engine)
}
