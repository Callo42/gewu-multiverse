//! The fx-r33 guards that need the new view/API surface.
//!
//! On `d99e6a1` this file does not compile: the copy route, the empty-list
//! evidence and the run-temp remover did not exist. The base-compatible half of
//! the guards lives in `fx-r33-guards.rs`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnding, RunEvidence};
use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
}

struct Ready(FakeEngine);

impl EngineApi for Ready {
    fn board(&mut self) -> BoardView {
        self.0.board()
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
        self.0.send(command)
    }
    fn tick(&mut self) {
        self.0.tick()
    }
    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.0.instruction_path(agent)
    }
    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.0.session_files(agent)
    }
    fn shutdown(&mut self, grace: Duration) {
        self.0.shutdown(grace)
    }
}

fn ready() -> FakeEngine {
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
    for _ in 0..140 {
        engine.tick();
    }
    let _ = engine.board();
    engine
}

fn decode_base64(text: &str) -> Vec<u8> {
    let value = |ch: char| -> u8 {
        match ch {
            'A'..='Z' => ch as u8 - b'A',
            'a'..='z' => ch as u8 - b'a' + 26,
            '0'..='9' => ch as u8 - b'0' + 52,
            '+' => 62,
            '/' => 63,
            _ => 0,
        }
    };
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for chunk in chars.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let a = value(chunk[0]) as u32;
        let b = value(chunk[1]) as u32;
        let c = value(chunk[2]) as u32;
        let d = value(chunk[3]) as u32;
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((triple >> 16) as u8);
        if chunk[2] != '=' {
            out.push((triple >> 8) as u8);
        }
        if chunk[3] != '=' {
            out.push(triple as u8);
        }
    }
    out
}

#[test]
fn the_copy_route_carries_the_password_without_printing_it_again() {
    let mut board = Board::new(Ready(ready()));
    press(&mut board, KeyCode::Char('f'));
    press(&mut board, KeyCode::Char('p'));
    let password = board
        .revealed_password()
        .expect("the password is on show")
        .to_owned();
    press(&mut board, KeyCode::Char('c'));
    let sequence = board.take_clipboard().expect("a copy sequence");
    assert!(
        sequence.starts_with("\x1b]52;c;") && sequence.ends_with('\x07'),
        "F-13: not an OSC 52 clipboard sequence: {sequence:?}"
    );
    let payload = &sequence["\x1b]52;c;".len()..sequence.len() - 1];
    let copied = String::from_utf8(decode_base64(payload)).expect("the copied text");
    assert_eq!(copied, password, "F-13: the wrong value was copied");
    assert!(
        !sequence.contains(&password),
        "F-13: the password was printed in plaintext a second time"
    );
}

#[test]
fn a_checked_empty_assignment_is_no_work_even_when_the_model_says_done() {
    let mut evidence = RunEvidence::default();
    evidence.note_forgejo_success(
        gewu_multiverse::forgejo_tools::LIST_ASSIGNED_ISSUES,
        "no open issue is assigned to `gewu-agent-1` in the cloned repositories",
    );
    assert_eq!(
        agent_run::ending_with_evidence("Everything assigned is done.", &evidence),
        RunEnding::NoWork,
        "U3: a checked, empty assignment was marked as success"
    );
    // A successful write means the run did work; an empty list no longer decides.
    let mut worked = RunEvidence::default();
    worked.note_forgejo_success(
        gewu_multiverse::forgejo_tools::LIST_ASSIGNED_ISSUES,
        "no open issue is assigned to `gewu-agent-1` in the cloned repositories",
    );
    worked.note_forgejo_success(gewu_multiverse::forgejo_tools::COMMENT, "commented on #1");
    assert_eq!(
        agent_run::ending_with_evidence("I commented on the issue.", &worked),
        RunEnding::Succeeded,
        "U3: a write after the empty list was still called no work"
    );
}

#[test]
fn a_runs_private_temp_is_removed_by_its_exact_pid_and_run_id() {
    let root = std::env::temp_dir().join(format!(
        "gewu-fx-r33-temp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the temp root");
    let doomed = gewu_multiverse::confinement::run_temp_path(&root, 4242, "run-1");
    let sibling = gewu_multiverse::confinement::run_temp_path(&root, 4243, "run-1");
    fs::create_dir_all(doomed.join("tmp")).expect("the doomed run temp");
    fs::write(doomed.join("tmp/askpass.toml"), b"synthetic").expect("the askpass file");
    fs::create_dir_all(&sibling).expect("the sibling run temp");
    gewu_multiverse::confinement::remove_run_temp_at(&root, 4242, "run-1");
    assert!(!doomed.exists(), "F-01: the run temp was left behind");
    assert!(sibling.exists(), "F-01: another run's temp was removed");
    let _ = fs::remove_dir_all(&root);
}

struct LongInstruction(FakeEngine);

impl EngineApi for LongInstruction {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.instruction_first_line = Some("The first file line is long.".to_owned());
            agent.instruction = Some(format!(
                "{}second file line\nthird file line\n",
                "a long first line ".repeat(30)
            ));
            agent.instruction_lines = 3;
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
            agent.recent = vec![gewu_multiverse::engine_api::SessionEnd::Succeeded];
            agent.last_result = Some("The review is complete.".to_owned());
        }
        view
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
        self.0.send(command)
    }
    fn tick(&mut self) {
        self.0.tick()
    }
    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.0.instruction_path(agent)
    }
    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.0.session_files(agent)
    }
    fn shutdown(&mut self, grace: Duration) {
        self.0.shutdown(grace)
    }
}

#[test]
fn the_instruction_marker_counts_file_lines_not_wrapped_rows() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut board = Board::new(LongInstruction(ready()));
    press(&mut board, KeyCode::Enter);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    let area = terminal.backend().buffer().area;
    let mut frame = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            frame.push_str(
                terminal
                    .backend()
                    .buffer()
                    .cell((x, y))
                    .expect("a cell")
                    .symbol(),
            );
        }
        frame.push('\n');
    }
    let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("(3 more lines)"),
        "F-11: the marker does not count the three file lines:\n{flat}"
    );
    assert!(
        !flat.contains("(12 more lines)"),
        "F-11: the marker counted wrapped display rows:\n{flat}"
    );
}
