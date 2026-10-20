//! Render tests for the board.
//!
//! Every screen is drawn at the four sizes the product promises, with 1, 3, 7 and
//! 20 agents where a screen can hold them. Every frame is checked for four things:
//! the product paints its own background on every cell; every card is whole; a wire
//! reaches every card; and every arm of every box-drawing cell meets a neighbour's
//! opposite arm. The connectivity guard is the one the correction asks for, and it
//! is run on the board, the picker preview and the build picture alike.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use gewu_multiverse::board::status::BANNED;
use gewu_multiverse::board::{Board, FakeEngine, Screen};
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Color;

const SIZES: [(u16, u16); 5] = [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)];
const COUNTS: [usize; 4] = [1, 3, 7, 20];

const L: u8 = 1;
const R: u8 = 2;
const U: u8 = 4;
const D: u8 = 8;

fn arms(ch: char) -> u8 {
    match ch {
        '─' => L | R,
        '│' => U | D,
        '╭' => R | D,
        '╮' => L | D,
        '╰' => R | U,
        '╯' => L | U,
        '├' => U | D | R,
        '┤' => U | D | L,
        '┬' => L | R | D,
        '┴' => L | R | U,
        '┼' => L | R | U | D,
        '╴' => L,
        '╶' => R,
        '╵' => U,
        '╷' => D,
        _ => 0,
    }
}

fn opposite(arm: u8) -> u8 {
    match arm {
        L => R,
        R => L,
        U => D,
        D => U,
        _ => 0,
    }
}

fn endpoint_engine(count: usize) -> FakeEngine {
    let mut engine = FakeEngine::new();
    engine
        .send(Command::AcknowledgeTerms)
        .expect("the terms are recorded");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("the endpoint is set");
    let first = count.min(12);
    engine
        .send(Command::CreateAgents { count: first })
        .expect("agents are created");
    for _ in first..count {
        engine.send(Command::AddAgent).expect("an agent is added");
    }
    engine.send(Command::StartForgejo).expect("Forgejo starts");
    for _ in 0..(120 + count * 2) {
        engine.tick();
    }
    let _ = engine.board();
    engine
}

fn running_engine(count: usize) -> FakeEngine {
    let mut engine = endpoint_engine(count);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    fs::write(path, "fix the flaky login test\n").expect("the instruction is written");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("the agent starts");
    engine.tick();
    let _ = engine.board();
    engine
}

fn blocked_engine() -> FakeEngine {
    let mut engine = endpoint_engine(1);
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("the agent starts");
    let _ = engine.board();
    engine
}

fn restarting_engine() -> FakeEngine {
    let mut engine = running_engine(1);
    for _ in 0..7 {
        engine.tick();
    }
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

fn fill_model(board: &mut Board, url: &str, model: &str, key: &str) {
    press(board, KeyCode::Char('e'));
    type_text(board, url);
    press(board, KeyCode::Enter);
    press(board, KeyCode::Down);
    press(board, KeyCode::Char('e'));
    type_text(board, model);
    press(board, KeyCode::Enter);
    press(board, KeyCode::Down);
    press(board, KeyCode::Char('e'));
    type_text(board, key);
    press(board, KeyCode::Enter);
}

fn draw(board: &mut Board, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

fn flat(buffer: &Buffer) -> String {
    text(buffer)
        .replace(['│', '╭', '╮', '╰', '╯', '─', '├', '┤', '┬', '┴', '┼'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn assert_painted(buffer: &Buffer, label: &str) {
    let area = buffer.area;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = buffer.cell((x, y)).expect("a cell");
            assert_ne!(
                cell.bg,
                Color::Reset,
                "{label}: cell ({x},{y}) was left on the terminal's background"
            );
            assert_ne!(
                cell.fg,
                Color::Reset,
                "{label}: cell ({x},{y}) was left on the terminal's default foreground"
            );
        }
    }
}

/// Every arm of every box-drawing cell must meet a neighbour's opposite arm.
fn assert_connected(buffer: &Buffer, label: &str) {
    let area = buffer.area;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = buffer.cell((x, y)).expect("a cell");
            let symbol = cell.symbol();
            let Some(ch) = symbol.chars().next() else {
                continue;
            };
            let cell_arms = arms(ch);
            if cell_arms == 0 || symbol.chars().count() != 1 {
                continue;
            }
            let neighbours = [
                (L, x.checked_sub(1), Some(y)),
                (R, Some(x + 1), Some(y)),
                (U, Some(x), y.checked_sub(1)),
                (D, Some(x), Some(y + 1)),
            ];
            for (arm, nx, ny) in neighbours {
                if cell_arms & arm == 0 {
                    continue;
                }
                let (Some(nx), Some(ny)) = (nx, ny) else {
                    panic!("{label}: {ch:?} at ({x},{y}) has an arm outside the window");
                };
                if nx >= area.right() || ny >= area.bottom() {
                    panic!("{label}: {ch:?} at ({x},{y}) has an arm outside the window");
                }
                let neighbour = buffer.cell((nx, ny)).expect("a neighbour");
                let neighbour_arms = neighbour
                    .symbol()
                    .chars()
                    .next()
                    .map(arms)
                    .unwrap_or_default();
                assert!(
                    neighbour_arms & opposite(arm) != 0,
                    "{label}: {ch:?} at ({x},{y}) points {arm:?} into {:?} at ({nx},{ny})",
                    neighbour.symbol()
                );
            }
        }
    }
}

fn assert_no_engine_words(buffer: &Buffer, label: &str) {
    let frame = text(buffer).to_ascii_lowercase();
    for word in BANNED {
        assert!(
            !frame.contains(word),
            "{label}: the screen shows the engine word {word:?}\n{frame}"
        );
    }
}

fn assert_cards_whole(board: &Board, buffer: &Buffer, label: &str) {
    let layout = board.layout();
    let area = buffer.area;
    for card in &layout.cards {
        let rect = card.rect;
        assert!(
            rect.x >= area.x && rect.right() <= area.right(),
            "{label}: card {} starts outside the window",
            card.agent
        );
        assert!(
            rect.y >= area.y && rect.bottom() <= area.bottom(),
            "{label}: card {} ends outside the window",
            card.agent
        );
        assert_eq!(
            buffer.cell((rect.x, rect.y)).expect("top left").symbol(),
            "╭",
            "{label}: card {} has no top-left corner",
            card.agent
        );
        assert_eq!(
            buffer
                .cell((rect.right() - 1, rect.y))
                .expect("top right")
                .symbol(),
            "╮",
            "{label}: card {} has no top-right corner",
            card.agent
        );
        assert_eq!(
            buffer
                .cell((rect.x, rect.bottom() - 1))
                .expect("bottom left")
                .symbol(),
            "╰",
            "{label}: card {} has no bottom-left corner",
            card.agent
        );
        assert_eq!(
            buffer
                .cell((rect.right() - 1, rect.bottom() - 1))
                .expect("bottom right")
                .symbol(),
            "╯",
            "{label}: card {} has no bottom-right corner",
            card.agent
        );
        for y in rect.y + 1..rect.bottom() - 1 {
            assert_eq!(
                buffer.cell((rect.x, y)).expect("left side").symbol(),
                "│",
                "{label}: card {} is cut on its left side",
                card.agent
            );
            assert_eq!(
                buffer
                    .cell((rect.right() - 1, y))
                    .expect("right side")
                    .symbol(),
                "│",
                "{label}: card {} is cut on its right side",
                card.agent
            );
        }
        let junction = (rect.x..rect.right())
            .find(|x| buffer.cell((*x, rect.y)).expect("top border").symbol() == "┴")
            .unwrap_or_else(|| panic!("{label}: card {} has no top junction", card.agent));
        let arrival = buffer.cell((junction, rect.y - 1)).expect("wire arrival");
        assert!(
            arms(arrival.symbol().chars().next().unwrap_or(' ')) & D != 0,
            "{label}: no wire arrives at card {} (found {:?})",
            card.agent,
            arrival.symbol()
        );
        assert!(
            layout.wire_cells.contains(&(junction, rect.y - 1)),
            "{label}: the wire at card {} is not recorded",
            card.agent
        );
    }
}

fn check_frame(board: &mut Board, width: u16, height: u16, label: &str) -> Buffer {
    let buffer = draw(board, width, height);
    assert_painted(&buffer, label);
    assert_connected(&buffer, label);
    assert_no_engine_words(&buffer, label);
    if board.screen() == Screen::Home || board.screen() == Screen::Build {
        assert!(!board.layout().cards.is_empty(), "{label}: no card drawn");
        assert_cards_whole(board, &buffer, label);
    }
    buffer
}

fn set_picker_count(board: &mut Board, count: usize) {
    let current = 3_usize;
    if count < current {
        for _ in 0..(current - count) {
            press(board, KeyCode::Left);
        }
    } else {
        for _ in 0..(count - current) {
            press(board, KeyCode::Right);
        }
    }
}

fn picker_board() -> Board {
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

fn build_board(count: usize, ticks: usize) -> Board {
    let mut board = picker_board();
    set_picker_count(&mut board, count);
    press(&mut board, KeyCode::Enter);
    for _ in 0..ticks {
        board.tick();
    }
    board
}

#[test]
fn every_screen_and_state_is_whole_at_every_size_and_count() {
    for (width, height) in SIZES {
        for count in COUNTS {
            let label = |name: &str| format!("{name} {width}x{height} with {count} agents");

            // Terms and model, first run.
            let mut board = Board::new(FakeEngine::new());
            check_frame(&mut board, width, height, &label("terms"));
            press(&mut board, KeyCode::Char('y'));
            assert_eq!(board.screen(), Screen::Model);
            check_frame(&mut board, width, height, &label("model-empty"));
            press(&mut board, KeyCode::Char('t'));
            // Picker.
            let picker_count = count.min(12);
            let mut board = picker_board();
            set_picker_count(&mut board, picker_count);
            check_frame(&mut board, width, height, &label("picker"));
            // Build: Forgejo is downloading.
            let mut board = build_board(picker_count, 6);
            assert_eq!(board.screen(), Screen::Build);
            check_frame(&mut board, width, height, &label("build-downloading"));
            // Home.
            let mut board = Board::new(endpoint_engine(count));
            assert_eq!(board.screen(), Screen::Home);
            check_frame(&mut board, width, height, &label("home"));
            press(&mut board, KeyCode::Up);
            assert_eq!(board.selection(), None);
            check_frame(&mut board, width, height, &label("home-forgejo-selected"));
            // Agent, stopped.
            let mut board = Board::new(endpoint_engine(count));
            press(&mut board, KeyCode::Enter);
            assert_eq!(board.screen(), Screen::Agent);
            check_frame(&mut board, width, height, &label("agent-stopped"));
            // Agent, running.
            let mut board = Board::new(running_engine(count));
            press(&mut board, KeyCode::Enter);
            check_frame(&mut board, width, height, &label("agent-running"));
            // Forgejo, masked and revealed.
            let mut board = Board::new(endpoint_engine(count));
            press(&mut board, KeyCode::Char('f'));
            assert_eq!(board.screen(), Screen::Forgejo);
            check_frame(&mut board, width, height, &label("forgejo"));
            press(&mut board, KeyCode::Char('p'));
            check_frame(&mut board, width, height, &label("forgejo-password"));
            // Model from the board.
            press(&mut board, KeyCode::Esc);
            press(&mut board, KeyCode::Char('m'));
            assert_eq!(board.screen(), Screen::Model);
            check_frame(&mut board, width, height, &label("model-board"));
            // Help.
            press(&mut board, KeyCode::Esc);
            press(&mut board, KeyCode::Char('?'));
            assert_eq!(board.screen(), Screen::Help);
            check_frame(&mut board, width, height, &label("help"));
            // The quit line, only with a started agent.
            let mut board = Board::new(running_engine(count));
            press(&mut board, KeyCode::Char('q'));
            assert!(!board.should_quit());
            let buffer = check_frame(&mut board, width, height, &label("quit-line"));
            assert!(flat(&buffer).contains("q again quits"));
        }
    }
}

#[test]
fn every_build_step_and_agent_state_is_checked() {
    for count in [1, 3, 7] {
        for (ticks, state) in [
            (0, "build-start"),
            (6, "build-42"),
            (16, "build-preparing"),
            (21, "build-starting"),
            (23, "build-running"),
        ] {
            let mut board = build_board(count, ticks);
            assert_eq!(board.screen(), Screen::Build, "{state} with {count}");
            check_frame(&mut board, 120, 40, &format!("{state} with {count}"));
        }
    }
    for count in [1, 7, 20] {
        let mut board = Board::new(blocked_engine());
        press(&mut board, KeyCode::Enter);
        let buffer = check_frame(&mut board, 120, 40, &format!("agent-blocked {count}"));
        assert!(
            flat(&buffer).contains("needs an instruction"),
            "{:?}",
            flat(&buffer)
        );

        let mut board = Board::new(restarting_engine());
        press(&mut board, KeyCode::Enter);
        let buffer = check_frame(&mut board, 120, 40, &format!("agent-restarting {count}"));
        assert!(flat(&buffer).contains("next run"), "{:?}", flat(&buffer));

        let mut engine = running_engine(count);
        for _ in 0..40 {
            engine.tick();
        }
        let _ = engine.board();
        let mut board = Board::new(engine);
        press(&mut board, KeyCode::Enter);
        let buffer = check_frame(&mut board, 120, 40, &format!("agent-strip {count}"));
        let frame = text(&buffer);
        assert!(frame.contains('✓') || frame.contains('✗'), "{frame}");
        for end in ['✓', '✗'] {
            if frame.contains(end) {
                break;
            }
        }
    }
}

#[test]
fn the_build_turns_connecting_into_logins_and_then_opens_the_board() {
    let mut board = build_board(3, 6);
    assert_eq!(board.screen(), Screen::Build);
    let buffer = draw(&mut board, 120, 40);
    let frame = text(&buffer);
    assert!(frame.contains("connecting…"), "{frame}");
    assert!(frame.contains("downloading 42%"), "{frame}");

    for _ in 6..25 {
        board.tick();
    }
    assert_eq!(board.screen(), Screen::Build);
    let buffer = draw(&mut board, 120, 40);
    let frame = text(&buffer);
    assert!(!frame.contains("connecting…"), "{frame}");
    assert!(frame.contains("gewu-agent-1"), "{frame}");
    assert!(frame.contains("gewu-agent-3"), "{frame}");
    assert!(frame.contains("● running"), "{frame}");

    board.tick();
    assert_eq!(board.screen(), Screen::Home);
}

#[test]
fn the_last_agent_scrolls_into_view_with_its_wire() {
    for (width, height) in SIZES {
        let mut board = Board::new(endpoint_engine(20));
        let _ = check_frame(&mut board, width, height, "home 20");
        for _ in 0..19 {
            press(&mut board, KeyCode::Right);
        }
        let buffer = check_frame(&mut board, width, height, "home 20 last");
        assert_eq!(board.selection(), Some(19));
        assert!(
            board.layout().cards.iter().any(|card| card.agent == 19),
            "the selected card is not drawn at {width}x{height}"
        );
        if board.layout().scrolls {
            assert!(
                flat(&buffer).contains("/ 20"),
                "the scroll line is missing at {width}x{height}"
            );
        }
    }
}

#[test]
fn the_terms_are_exact_and_all_shown_before_y_records_them() {
    let expected = [
        "Agents use this computer with your permissions.",
        "They can read your files, including saved keys.",
        "Each has its own login on your Forgejo.",
        "Started agents keep going until you stop them.",
        "Their work is sent to your model provider.",
    ];
    for (width, height) in SIZES {
        let mut board = Board::new(FakeEngine::new());
        assert_eq!(board.view().terms, expected);
        press(&mut board, KeyCode::Char('y'));
        assert!(
            !board
                .sent_commands()
                .iter()
                .any(|command| matches!(command, Command::AcknowledgeTerms)),
            "y was accepted before the items were drawn"
        );
        let buffer = draw(&mut board, width, height);
        for item in expected {
            assert!(
                text(&buffer).contains(item),
                "the term {item:?} was not on screen at {width}x{height}"
            );
        }
        press(&mut board, KeyCode::Char('y'));
        assert_eq!(board.sent_commands(), [Command::AcknowledgeTerms]);
        assert_eq!(board.screen(), Screen::Model);
    }
}

#[test]
fn the_board_fills_the_middle_of_a_100x30_window() {
    let screens = [
        ("terms", Board::new(FakeEngine::new())),
        ("model", {
            let mut board = Board::new(FakeEngine::new());
            let _ = draw(&mut board, 100, 30);
            press(&mut board, KeyCode::Char('y'));
            board
        }),
        ("picker", picker_board()),
        ("build", build_board(7, 6)),
        ("home", Board::new(endpoint_engine(3))),
        ("agent", {
            let mut board = Board::new(running_engine(3));
            press(&mut board, KeyCode::Enter);
            board
        }),
        ("forgejo", {
            let mut board = Board::new(endpoint_engine(3));
            press(&mut board, KeyCode::Char('f'));
            board
        }),
        ("help", {
            let mut board = Board::new(endpoint_engine(3));
            press(&mut board, KeyCode::Char('?'));
            board
        }),
    ];
    for (name, mut board) in screens {
        let buffer = draw(&mut board, 100, 30);
        let area = buffer.area;
        let mut first = None;
        let mut last = 0;
        for y in area.y + 1..area.bottom().saturating_sub(4) {
            let mut used = false;
            for x in area.x + 2..area.right().saturating_sub(2) {
                if buffer.cell((x, y)).expect("a cell").symbol() != " " {
                    used = true;
                    break;
                }
            }
            if used {
                first.get_or_insert(y);
                last = y;
            }
        }
        let span = first.map(|first| last - first + 1).unwrap_or(0);
        assert!(
            span >= 12,
            "{name} only uses {span} rows of the 100x30 middle"
        );
    }
}

#[test]
fn a_stopped_board_quits_without_a_quit_line() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('q'));
    assert!(board.should_quit());
    assert_eq!(board.shutdown_calls(), 1);
    assert!(
        !board
            .sent_commands()
            .iter()
            .any(|command| matches!(command, Command::Stop { .. }))
    );
}

#[test]
fn the_help_screen_stays_under_forty_words() {
    let mut board = Board::new(endpoint_engine(3));
    press(&mut board, KeyCode::Char('?'));
    let buffer = draw(&mut board, 100, 30);
    let body = text(&buffer).replace(['│', '╭', '╮', '╰', '╯', '─', '├', '┤', '┬', '┴', '┼'], " ");
    let words = body
        .split_whitespace()
        .filter(|word| word.chars().filter(|ch| ch.is_alphabetic()).count() >= 2)
        .count();
    assert!(words <= 40, "help has {words} words:\n{body}");
}

#[test]
fn every_extra_model_and_agent_state_is_connected_at_every_size() {
    for (width, height) in SIZES {
        // Model failure and success.
        let mut board = Board::new(FakeEngine::new());
        let _ = draw(&mut board, width, height);
        press(&mut board, KeyCode::Char('y'));
        fill_model(
            &mut board,
            "https://model.example/v1",
            "demo-model",
            "demo-key",
        );
        press(&mut board, KeyCode::Char('t'));
        board.tick();
        check_frame(&mut board, width, height, "model-fail");
        press(&mut board, KeyCode::Up);
        press(&mut board, KeyCode::Up);
        press(&mut board, KeyCode::Char('e'));
        type_text(&mut board, "/chat/completions");
        press(&mut board, KeyCode::Enter);
        press(&mut board, KeyCode::Char('t'));
        board.tick();
        check_frame(&mut board, width, height, "model-pass");

        // Every build step, at 1, 3 and 7 agents.
        for count in [1, 3, 7] {
            for ticks in [0, 6, 16, 21, 23] {
                let mut board = build_board(count, ticks);
                check_frame(
                    &mut board,
                    width,
                    height,
                    &format!("build {ticks} ticks with {count}"),
                );
            }
        }

        // Blocked, restarting and the strip.
        let mut board = Board::new(blocked_engine());
        press(&mut board, KeyCode::Enter);
        check_frame(&mut board, width, height, "agent-blocked");

        let mut board = Board::new(restarting_engine());
        press(&mut board, KeyCode::Enter);
        check_frame(&mut board, width, height, "agent-restarting");

        let mut engine = running_engine(1);
        for _ in 0..40 {
            engine.tick();
        }
        let _ = engine.board();
        let mut board = Board::new(engine);
        press(&mut board, KeyCode::Enter);
        check_frame(&mut board, width, height, "agent-strip");

        // Twelve preview cards and the full picker.
        let mut board = picker_board();
        set_picker_count(&mut board, 12);
        check_frame(&mut board, width, height, "picker-12");
    }
}

/// An engine whose next picture carries the corrections Main asked for: one
/// repository, and a notice with the engine's backticks around the names.
struct CorrectionEngine(FakeEngine);

impl EngineApi for CorrectionEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.repositories = 1;
        view.notice = Some("`agent-4` is connected as `gewu-agent-4`".to_owned());
        view
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        self.0.send(command)
    }

    fn tick(&mut self) {
        self.0.tick();
    }

    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.0.instruction_path(agent)
    }

    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.0.session_files(agent)
    }

    fn shutdown(&mut self, grace: Duration) {
        self.0.shutdown(grace);
    }
}

#[test]
fn the_corrections_main_asked_for_are_on_the_board() {
    for (width, height) in [(72, 18), (100, 30), (120, 40)] {
        for count in [1, 3, 4, 7] {
            let mut board = Board::new(CorrectionEngine(endpoint_engine(count)));
            let buffer = draw(&mut board, width, height);
            let label = format!("{width}x{height} with {count}");
            let flat = flat(&buffer);

            if height >= 30 {
                assert!(
                    flat.contains("team · 1 repository") && !flat.contains("1 repositories"),
                    "{label}: {flat}"
                );
            }
            assert!(
                flat.contains("agent-4 is connected as gewu-agent-4"),
                "{label}: the notice is not plain: {flat}"
            );
            assert!(!flat.contains('`'), "{label}: a backtick was drawn");

            // The first card keeps the three-card row's one-cell margin.
            let first = board
                .layout()
                .cards
                .iter()
                .map(|card| card.rect.x)
                .min()
                .expect("a card is drawn");
            assert!(first >= 2, "{label}: the first card touches the frame");

            // No empty row inside the Forgejo box: every row below the accent bar
            // carries one of its facts.
            if let Some(forgejo) = board.layout().forgejo {
                for y in (forgejo.y + 2)..forgejo.bottom().saturating_sub(1) {
                    let row: String = (forgejo.x + 1..forgejo.right().saturating_sub(1))
                        .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "))
                        .collect();
                    assert!(
                        !row.trim().is_empty(),
                        "{label}: empty row {y} inside the Forgejo box"
                    );
                }
            }

            if height >= 30 {
                assert!(
                    flat.contains("your login:"),
                    "{label}: the login line has no colon: {flat}"
                );
            }
        }
    }
}
