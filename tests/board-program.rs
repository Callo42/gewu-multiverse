use gewu_multiverse::board::Board;
use gewu_multiverse::board::FakeEngine;
use gewu_multiverse::engine_api::{Command, EngineApi};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn an_agent_can_choose_and_install_a_program_from_its_screen() {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("demo".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('p'));

    // Built-in is the first row; moving down reaches Pi.
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Enter);
    assert!(board.sent_commands().iter().any(|command| matches!(
        command,
        Command::SetAgentProgram { program, .. } if program == "pi"
    )));

    press(&mut board, KeyCode::Char('p'));
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Char('i'));
    assert!(board.sent_commands().iter().any(|command| matches!(
        command,
        Command::InstallProgram { id } if id == "pi"
    )));
}

#[test]
fn codex_full_access_needs_a_second_explicit_keypress() {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("demo".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('p'));
    // Built-in, Pi, Codex.
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Char('f'));
    assert!(
        !board
            .sent_commands()
            .iter()
            .any(|command| matches!(command, Command::SetAgentCodexSandbox { .. }))
    );
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("Landlock"))
    );
    press(&mut board, KeyCode::Char('f'));
    assert!(board.sent_commands().iter().any(|command| matches!(
        command,
        Command::SetAgentCodexSandbox { mode, .. } if mode == "full-access"
    )));
}
