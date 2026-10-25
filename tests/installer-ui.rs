//! The installer's narrow board copy: progress, cancel and quit stay self-contained
//! at the two supported floor sizes (72×18 and 80×24).

use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine_api::{Command, EngineApi, ProgramView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn flat(terminal: &Terminal<TestBackend>) -> String {
    text(terminal)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area: Rect = buffer.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

fn program(
    id: &str,
    label: &str,
    installed: bool,
    installing: Option<Option<u8>>,
    phase: Option<&str>,
    error: Option<&str>,
) -> ProgramView {
    ProgramView {
        id: id.to_owned(),
        label: label.to_owned(),
        version: match id {
            "pi" => "1.4.0",
            "codex" => "0.164.0",
            "dsh" => "0.2.1-rc.2",
            _ => "2.1.4",
        }
        .to_owned(),
        installed_version: installed.then(|| "1.4.0".to_owned()),
        installed,
        license: "MIT".to_owned(),
        download_mb: match id {
            "pi" => 43,
            "codex" => 162,
            "dsh" => 753,
            _ => 240,
        },
        prerequisite: (id == "dsh").then(|| "Node 24.9+ / npm 11.16+".to_owned()),
        prerequisite_met: true,
        installing,
        installing_phase: phase.map(str::to_owned),
        error: error.map(str::to_owned),
    }
}

fn board_with(programs: Vec<ProgramView>) -> Board {
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
    engine.set_programs(programs);
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    press(&mut board, KeyCode::Char('p'));
    board
}

fn draw(board: &mut Board, width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    terminal
}

#[test]
fn installer_progress_and_cancel_copy_fit_the_floor_sizes() {
    let mut board = board_with(vec![
        program("pi", "Pi", false, Some(Some(37)), Some("downloading"), None),
        program("codex", "Codex", true, None, None, None),
        program("dsh", "DeepSeek Harness", false, None, None, None),
        program("opencode", "OpenCode", false, None, None, None),
    ]);
    press(&mut board, KeyCode::Down);
    for (width, height) in [(72, 18), (80, 24)] {
        let terminal = draw(&mut board, width, height);
        let picture = flat(&terminal);
        assert!(
            picture.contains("Agent program"),
            "{width}×{height}\n{picture}"
        );
        assert!(
            picture.contains("downloading"),
            "{width}×{height}\n{picture}"
        );
        assert!(picture.contains("37%"), "{width}×{height}\n{picture}");
        assert!(
            picture.contains("i cancel install"),
            "{width}×{height}\n{picture}"
        );
        assert!(
            !picture.contains("import"),
            "the installer copy leaked an implementation word: {width}×{height}\n{picture}"
        );
    }

    // The cancel key is the same key that started the install.
    press(&mut board, KeyCode::Char('i'));
    assert!(board.sent_commands().iter().any(|command| matches!(
        command,
        Command::CancelInstallProgram { id } if id == "pi"
    )));
}

#[test]
fn phases_without_a_percentage_are_named_at_the_floor_size() {
    let mut board = board_with(vec![
        program(
            "pi",
            "Pi",
            false,
            Some(None),
            Some("installing packages"),
            None,
        ),
        program("codex", "Codex", true, None, None, None),
        program("dsh", "DeepSeek Harness", false, None, None, None),
        program("opencode", "OpenCode", false, None, None, None),
    ]);
    press(&mut board, KeyCode::Down);
    let terminal = draw(&mut board, 72, 18);
    let picture = flat(&terminal);
    assert!(picture.contains("installing packages"), "{picture}");
    assert!(picture.contains("i cancel install"), "{picture}");
}

#[test]
fn an_error_names_the_old_version_and_a_quit_cancels_the_selected_install() {
    let mut board = board_with(vec![
        program(
            "pi",
            "Pi",
            false,
            None,
            None,
            Some("package checksum mismatch; the old version is still installed"),
        ),
        program("codex", "Codex", true, None, None, None),
        program("dsh", "DeepSeek Harness", false, None, None, None),
        program("opencode", "OpenCode", false, None, None, None),
    ]);
    press(&mut board, KeyCode::Down);
    let terminal = draw(&mut board, 72, 18);
    let picture = flat(&terminal);
    assert!(picture.contains("checksum mismatch"), "{picture}");
    assert!(
        picture.contains("old version is still installed"),
        "{picture}"
    );

    // Select DeepSeek Harness: its prerequisite must be whole and useful at 72.
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Down);
    let terminal = draw(&mut board, 72, 18);
    let picture = flat(&terminal);
    assert!(picture.contains("Node 24.9+ / npm 11.16+"), "{picture}");
    assert!(picture.contains("npm"), "{picture}");

    // Back to Pi and start its install, then quit once: the warning must say what
    // q again does rather than claim only Forgejo/agents stop.
    press(&mut board, KeyCode::Up);
    press(&mut board, KeyCode::Up);
    press(&mut board, KeyCode::Char('i'));
    board.tick();
    assert!(board.installing_count() == 1);
    board.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("previous version"))
    );
    let terminal = draw(&mut board, 72, 18);
    let picture = flat(&terminal);
    assert!(
        picture.contains("cancels installing Pi") || picture.contains("cancels 1 install"),
        "{picture}"
    );
    assert!(picture.contains("previous version is kept"), "{picture}");
    board.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(board.should_quit());
}
