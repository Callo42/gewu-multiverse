//! Render the new Program screen at the required size/count matrix. Run with
//! `GEWU_CAPTURE_DIR` set; no terminal or network is needed.

use std::path::PathBuf;

use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine_api::{Command, EngineApi};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn capture_the_program_screen_at_the_required_matrix() {
    let Ok(dir) = std::env::var("GEWU_CAPTURE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("capture directory");
    for count in [1, 4, 7, 12] {
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
            .send(Command::CreateAgents { count })
            .expect("agents");
        let mut board = Board::new(engine);
        press(&mut board, KeyCode::Enter);
        press(&mut board, KeyCode::Char('p'));
        for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal.draw(|frame| board.draw(frame)).expect("draw");
            let buffer = terminal.backend().buffer();
            let mut text = String::new();
            for y in 0..height {
                for x in 0..width {
                    text.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
                }
                text.push('\n');
            }
            std::fs::write(
                dir.join(format!("program-{count}-{width}x{height}.txt")),
                text,
            )
            .expect("capture");
        }
    }
}
