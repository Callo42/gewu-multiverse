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
use gewu_multiverse::board::{Board, BoardForm, FakeEngine, Screen};
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
    type_text(board, model);
    press(board, KeyCode::Enter);
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
    // The one-line form is one wired box whose interior holds every agent line.
    // Each line is one row and stays inside the frame; the box is checked by the
    // connectivity guard, and the list's own guard checks the names.
    for line in &layout.listed {
        let rect = line.rect;
        assert_eq!(rect.height, 1, "{label}: a list line is not one row");
        assert!(
            rect.x >= area.x && rect.right() <= area.right(),
            "{label}: list line {} starts outside the window",
            line.agent
        );
        assert!(
            rect.y >= area.y && rect.bottom() <= area.bottom(),
            "{label}: list line {} ends outside the window",
            line.agent
        );
        let row: String = (rect.x..rect.right())
            .map(|x| buffer.cell((x, rect.y)).expect("a cell").symbol())
            .collect();
        assert!(
            !row.trim().is_empty(),
            "{label}: list line {} is blank",
            line.agent
        );
    }
}

fn check_frame(board: &mut Board, width: u16, height: u16, label: &str) -> Buffer {
    let buffer = draw(board, width, height);
    assert_painted(&buffer, label);
    assert_connected(&buffer, label);
    assert_no_engine_words(&buffer, label);
    if board.screen() == Screen::Home || board.screen() == Screen::Build {
        let layout = board.layout();
        assert!(
            !layout.cards.is_empty() || !layout.listed.is_empty(),
            "{label}: no card or list line drawn"
        );
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
        let layout = board.layout();
        assert!(
            layout.cards.iter().any(|card| card.agent == 19)
                || layout.listed.iter().any(|line| line.agent == 19),
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
        "Where runs are confined, they cannot read the product's keys, your home or another agent's files; they start with a cleared environment and still see /etc, /proc, /sys, /run, the installed tools and the network. The automatic confinement is Landlock; bwrap and unshare need GEWU_CONFINEMENT and are tested, not yet proven on a real host.",
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
        // A warning may wrap at a narrow size; compare the screen with its line
        // breaks removed, not a row against the whole sentence.
        let drawn = flat(&buffer);
        for item in expected {
            assert!(
                drawn.contains(item),
                "the term {item:?} was not on screen at {width}x{height}:\n{drawn}"
            );
        }
        press(&mut board, KeyCode::Char('y'));
        assert_eq!(board.sent_commands(), [Command::AcknowledgeTerms]);
        assert_eq!(board.screen(), Screen::Model);
    }
}

#[test]
fn the_sixth_warning_is_drawn_at_every_size_too() {
    // On a host without confinement the terms list has one more item; the 72x18
    // floor must still show every warning before `y` records them.
    for (width, height) in SIZES {
        let mut board = Board::new(FakeEngine::unconfined());
        assert!(!board.view().confined);
        assert_eq!(board.view().terms.len(), 6);
        let expected = board.view().terms.clone();
        press(&mut board, KeyCode::Char('y'));
        assert!(
            !board
                .sent_commands()
                .iter()
                .any(|command| matches!(command, Command::AcknowledgeTerms)),
            "y was accepted before the six items were drawn"
        );
        let buffer = draw(&mut board, width, height);
        let drawn = flat(&buffer);
        for item in &expected {
            assert!(
                drawn.contains(item),
                "the term {item:?} was not on screen at {width}x{height}:\n{drawn}"
            );
        }
        assert!(drawn.contains("Runs are not confined on this computer."));
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
            let layout = board.layout();
            let first = layout
                .cards
                .iter()
                .chain(layout.listed.iter())
                .map(|card| card.rect.x)
                .min()
                .expect("a card or list line is drawn");
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

/// One row with a card's section rule on it must carry no text: the rule is a row of
/// its own, and the instruction is truncated inside its content area (F1).
fn assert_no_text_on_a_rule_row(buffer: &Buffer, label: &str) {
    let area = buffer.area;
    for y in area.y..area.bottom() {
        let row: Vec<&str> = (area.x..area.right())
            .map(|x| buffer.cell((x, y)).expect("a cell").symbol())
            .collect();
        let Some(left) = row.iter().position(|symbol| *symbol == "├") else {
            continue;
        };
        let Some(right) = row.iter().rposition(|symbol| *symbol == "┤") else {
            continue;
        };
        if right <= left + 1 {
            continue;
        }
        for symbol in &row[left + 1..right] {
            let acceptable = symbol.chars().all(|ch| {
                ch.is_whitespace() || matches!(ch, '─' | '┬' | '┴' | '┼' | '├' | '┤' | '╴' | '╶')
            });
            assert!(
                acceptable,
                "{label}: text {symbol:?} shares row {y} with a section rule:\n{}",
                text(buffer)
            );
        }
    }
}

fn four_line_engine(count: usize) -> FakeEngine {
    let mut engine = endpoint_engine(count);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    fs::write(
        path,
        "Take issue #1 in team/website: the README install command is out of date.\n\
         Change the README to use the published one-line installer,\n\
         commit the change on a new branch, open a pull request explaining the fix,\n\
         and comment on issue #1 with the pull request link.\n",
    )
    .expect("the instruction is written");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("the agent starts");
    engine.tick();
    let _ = engine.board();
    engine
}

#[test]
fn no_card_text_shares_a_row_with_a_section_rule_at_any_size() {
    for (width, height) in SIZES {
        for (name, engine, second) in [
            ("stopped", endpoint_engine(3), false),
            ("running", four_line_engine(3), false),
            ("blocked", blocked_engine(), false),
            ("restarting", restarting_engine(), false),
            (
                "failed",
                {
                    let mut engine = four_line_engine(1);
                    for _ in 0..40 {
                        engine.tick();
                    }
                    let _ = engine.board();
                    engine
                },
                true,
            ),
        ] {
            let mut board = Board::new(engine);
            press(&mut board, KeyCode::Enter);
            if second {
                // Open agent-1; its newest run failed in the fake's cycle.
                assert_eq!(board.screen(), Screen::Agent);
            }
            let buffer = draw(&mut board, width, height);
            assert_no_text_on_a_rule_row(&buffer, &format!("{name} {width}x{height}"));
            // The instruction is still named, whole or with its remaining-line count.
            let flat = flat(&buffer);
            assert!(
                flat.contains("instruction"),
                "{name} {width}x{height}: no instruction section: {flat}"
            );
        }
    }
}

#[test]
fn the_72_column_board_keeps_every_agent_on_the_frame() {
    // One vertical list of compact cards while the cards fit (F7): the wire above
    // each card leaves the box or the previous card's bottom centre.
    for count in [1, 3] {
        let mut board = Board::new(endpoint_engine(count));
        let buffer = draw(&mut board, 72, 18);
        let layout = board.layout();
        assert_eq!(
            layout.form,
            BoardForm::Cards,
            "72x18 with {count} agents is not the card form"
        );
        assert_eq!(
            layout.columns, 1,
            "72x18 with {count} agents is not a single column"
        );
        let mut x: Option<u16> = None;
        let mut last_bottom = 0;
        for card in &layout.cards {
            if let Some(expected) = x {
                assert_eq!(card.rect.x, expected, "the vertical list moves sideways");
            } else {
                x = Some(card.rect.x);
            }
            assert!(
                card.rect.y >= last_bottom,
                "cards overlap in the vertical list"
            );
            last_bottom = card.rect.bottom();
            let below = buffer
                .cell((card.rect.x + card.rect.width / 2 + 2, card.rect.y - 1))
                .expect("the wire above a card")
                .symbol();
            assert!(
                matches!(below, "│" | "┴" | "╵"),
                "the vertical list has no spine above card {}: {below:?}",
                card.agent
            );
        }
        assert!(!layout.cards.is_empty());
    }
    // When the cards cannot all fit, the same place becomes the one-line form and
    // no agent is left off the frame (B1). Each line is one row.
    for count in [4, 7, 12] {
        let mut board = Board::new(endpoint_engine(count));
        let buffer = draw(&mut board, 72, 18);
        let frame = text(&buffer);
        let layout = board.layout();
        assert_eq!(
            layout.form,
            BoardForm::List,
            "72x18 with {count} agents did not take the one-line form"
        );
        assert_eq!(layout.listed.len(), count, "a list line is missing");
        assert!(
            !layout.scrolls,
            "the 72x18 list scrolls with {count} agents"
        );
        let words: Vec<&str> = frame.split_whitespace().collect();
        for number in 1..=count {
            assert!(
                words.contains(&format!("agent-{number}").as_str()),
                "agent-{number} is off the 72x18 frame with {count} agents:\n{frame}"
            );
            assert!(
                words.contains(&format!("gewu-agent-{number}").as_str()),
                "gewu-agent-{number}'s login is off the frame with {count} agents:\n{frame}"
            );
        }
    }
}

#[test]
fn a_wide_window_caps_the_picture_at_about_one_hundred_columns() {
    let mut board = Board::new(endpoint_engine(3));
    let buffer = draw(&mut board, 200, 60);
    let area = buffer.area;
    let mut left = area.right();
    let mut right = area.x;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if buffer.cell((x, y)).expect("a cell").symbol() != " " {
                left = left.min(x);
                right = right.max(x);
            }
        }
    }
    // The frame is 160 wide, centred: the drawn cells sit inside 20..180 of 200, and
    // the extra width is used rather than left as margin.
    assert!(
        left >= 15 && right <= 185,
        "the 200x60 picture spans {left}..{right}"
    );
    assert!(
        right - left <= 164,
        "the picture is {} columns wide",
        right - left
    );
    // A 120-column window still uses its room.
    let mut board = Board::new(endpoint_engine(3));
    let buffer = draw(&mut board, 120, 40);
    let area = buffer.area;
    let mut left = area.right();
    let mut right = area.x;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if buffer.cell((x, y)).expect("a cell").symbol() != " " {
                left = left.min(x);
                right = right.max(x);
            }
        }
    }
    assert!(right - left > 100, "the 120x40 picture is capped too");
}

/// An engine that shows a long password when `p` asks, so the 80-column reveal can be
/// checked for the whole value and never an ellipsis.
struct LongPasswordEngine {
    inner: FakeEngine,
    revealed: bool,
}

impl EngineApi for LongPasswordEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.inner.board();
        if self.revealed {
            view.admin_password = Some(
                "Gw-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef!9".to_owned(),
            );
        }
        view
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        if matches!(command, Command::RevealAdminPassword) {
            self.revealed = true;
        }
        self.inner.send(command)
    }

    fn tick(&mut self) {
        self.inner.tick();
    }

    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.inner.instruction_path(agent)
    }

    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.inner.session_files(agent)
    }

    fn shutdown(&mut self, grace: Duration) {
        self.inner.shutdown(grace);
    }
}

#[test]
fn the_revealed_password_is_whole_at_eighty_columns() {
    let password = "Gw-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef!9";
    for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40)] {
        let mut board = Board::new(LongPasswordEngine {
            inner: endpoint_engine(3),
            revealed: false,
        });
        press(&mut board, KeyCode::Char('f'));
        press(&mut board, KeyCode::Char('p'));
        let frame = text(&draw(&mut board, width, height));
        // The page wraps the value; the whole secret is on screen, never cut short.
        let flat: String = frame
            .replace(['│', '╭', '╮', '╰', '╯', '─'], "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("");
        assert!(
            flat.contains(password),
            "{width}x{height}: the revealed password is not whole:\n{frame}"
        );
        assert!(
            !frame.contains("••••••••   p"),
            "{width}x{height}: the password was not revealed:\n{frame}"
        );
    }
}

/// An engine whose view carries the facts the walk found missing: repository names,
/// the permission summary, the team address, a kept login, repository events, and a
/// card's work block, remaining instruction lines and last failure.
struct RichEngine(FakeEngine);

impl EngineApi for RichEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.repository_names =
            vec!["team/workspace".to_owned(), "team/website".to_owned()];
        view.forgejo.events = vec!["team/scratch was deleted".to_owned()];
        view.forgejo.admin_login = "mei".to_owned();
        view.forgejo.admin_login_kept = true;
        if let Some(agent) = view.agents.first_mut() {
            agent.instruction_lines = 5;
            agent.instruction_first_line = Some(
                "Take issue #1 in team/website: the README install command is out of date."
                    .to_owned(),
            );
            agent.last_work = vec![
                "issue #2".to_owned(),
                "pull request #5 from fix/contributing-link".to_owned(),
                "comment on #2".to_owned(),
            ];
            agent.last_failure = Some("shell: exit 128: fatal: could not read".to_owned());
            agent.activity = Some("opened pull request #5".to_owned());
            agent.activity_at = Some(std::time::SystemTime::now() - Duration::from_secs(120));
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
        }
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

/// Whether the frame contains `needle` across a wrapped line boundary.
fn flat_contains(frame: &str, needle: &str) -> bool {
    frame
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .contains(needle)
}

#[test]
fn the_forgejo_page_names_repositories_permissions_and_the_team() {
    let mut board = Board::new(RichEngine(endpoint_engine(3)));
    press(&mut board, KeyCode::Char('f'));
    let frame = text(&draw(&mut board, 120, 40));
    assert!(frame.contains("team/workspace"), "{frame}");
    assert!(frame.contains("team/website"), "{frame}");
    assert!(frame.contains("write to team repositories only"), "{frame}");
    assert!(frame.contains("/org/team/teams/agents"), "{frame}");
    assert!(
        frame.contains("only on issues assigned to them — assign an issue in Forgejo"),
        "{frame}"
    );
    assert!(frame.contains("kept from an earlier setup"), "{frame}");
    assert!(frame.contains("team/scratch was deleted"), "{frame}");
    // The board's own box names the repositories at a roomy size.
    press(&mut board, KeyCode::Esc);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(frame.contains("team/website"), "{frame}");
}

#[test]
fn a_card_names_its_last_work_and_its_remaining_instruction() {
    let mut board = Board::new(RichEngine(endpoint_engine(3)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(flat_contains(&frame, "issue #2"), "{frame}");
    assert!(
        flat_contains(&frame, "pull request #5 from fix/contributing-link"),
        "{frame}"
    );
    assert!(flat_contains(&frame, "comment on #2"), "{frame}");
    assert!(frame.contains("more lines"), "{frame}");
    // The last failure is on the card; the stale activity is marked `last:`.
    assert!(frame.contains("⚠ shell:"), "{frame}");
    let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("last: opened pull request #5"), "{flat}");
    assert!(flat.contains("2m ago"), "{flat}");
}

/// An engine whose newest run failed at the model call: the card carries the first
/// clause of that error, not only a `✗` mark (P6).
struct ErrorEngine(FakeEngine);

impl EngineApi for ErrorEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
            agent.activity = None;
            agent.last_failure = None;
            agent.last_error = Some(
                "the model call failed (transport) at model http://127.0.0.1:1 — could not be reached"
                    .to_owned(),
            );
        }
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
fn the_last_errors_first_clause_is_on_the_card() {
    let mut board = Board::new(ErrorEngine(endpoint_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 100, 30));
    let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("✗ the model call failed (transport) at model http://127.0.0.1:1"),
        "{flat}"
    );
    assert!(
        !flat.contains("could not be reached"),
        "the second clause is on the card: {flat}"
    );
}

/// An engine on a host that offers no confinement: the board must say so in words.
struct UnconfinedEngine(FakeEngine);

impl EngineApi for UnconfinedEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.confined = false;
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
fn an_unconfined_host_says_so_on_the_board() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut board = Board::new(UnconfinedEngine(endpoint_engine(3)));
        let buffer = draw(&mut board, width, height);
        let frame = text(&buffer);
        assert!(
            frame.contains("runs are not confined on this computer"),
            "{width}x{height}: the board does not say so:\n{frame}"
        );
    }
}

/// One physical instruction line long enough to need signposting at the floor.
fn long_line_engine(count: usize) -> FakeEngine {
    let mut engine = endpoint_engine(count);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    fs::write(
        path,
        "Take issue #1 in team/website: the README install command is out of date and the published one-line installer is the one to use; change the README, commit the change on a branch, open a pull request explaining the fix, and comment on issue #1 with the link.\n",
    )
    .expect("the instruction is written");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("the agent starts");
    engine.tick();
    let _ = engine.board();
    engine
}

#[test]
fn a_long_one_line_instruction_is_signposted_at_the_floor() {
    for (width, height) in [(72, 18), (80, 24)] {
        let mut board = Board::new(long_line_engine(1));
        press(&mut board, KeyCode::Enter);
        let buffer = draw(&mut board, width, height);
        let frame = text(&buffer);
        let flat = flat(&buffer);
        // U4/F-11: the first display row is always drawn. When the whole line
        // fits, it is whole; when it does not, the first row stays and a marker
        // (where a row can be spared) counts the *file* lines not shown.
        assert!(
            flat.contains("Take issue #1"),
            "{width}x{height}: the first line disappeared behind the count:\n{frame}"
        );
        if !flat.contains("with the link.") && width >= 80 {
            assert!(
                frame.contains("… (") && frame.contains("more line"),
                "{width}x{height}: the dropped text is not counted:\n{frame}"
            );
        }
        assert_no_text_on_a_rule_row(&buffer, &format!("long-line {width}x{height}"));
    }
}

#[test]
fn a_long_one_line_instruction_keeps_its_first_line_when_a_second_row_exists() {
    // At 80x24 there are four content rows: the first three carry the instruction's
    // first display rows and the fourth carries the marker. At the 72x18 floor only
    // one row exists and it is the marker (N9's documented marker-only form).
    let mut engine = long_line_engine(1);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    let one = fs::read_to_string(&path).expect("the instruction");
    fs::write(&path, format!("{one}{one}")).expect("the long instruction is written");
    engine.tick();
    let _ = engine.board();

    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    let buffer = draw(&mut board, 80, 24);
    let frame = text(&buffer);
    let first = frame
        .lines()
        .position(|line| line.contains("Take issue #1"))
        .unwrap_or_else(|| panic!("the first line is missing at 80x24:\n{frame}"));
    let marker = frame
        .lines()
        .position(|line| line.contains("more line"))
        .unwrap_or_else(|| panic!("the marker is missing at 80x24:\n{frame}"));
    assert!(
        first < marker,
        "the marker is not after the first line:\n{frame}"
    );
    assert_no_text_on_a_rule_row(&buffer, "long-line first-row 80x24");

    let mut board = Board::new(long_line_engine(1));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 72, 18));
    assert!(
        frame.contains("Take issue #1"),
        "the one-row floor must keep the first line (U4):\n{frame}"
    );
}

/// An engine with more than five run endings, so the bounded strip and its `+N` are
/// on the Agent screen.
struct StripEngine(FakeEngine);

impl EngineApi for StripEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Failed {
                    reason: "the model call failed".to_owned(),
                },
                gewu_multiverse::engine_api::SessionEnd::TimedOut,
                gewu_multiverse::engine_api::SessionEnd::Stopped,
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
            ];
        }
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
fn the_run_strip_is_bounded_with_a_legend_and_a_count() {
    let mut board = Board::new(StripEngine(running_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 120, 40));
    let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("last runs"), "{flat}");
    assert!(flat.contains("+4"), "no +N for four hidden marks: {flat}");
    assert!(
        flat.contains("✓ ok") && flat.contains("✗ failed"),
        "no legend: {flat}"
    );
    assert!(flat.contains("run "), "no run count: {flat}");
}

struct SuccessResultEngine(FakeEngine);

impl EngineApi for SuccessResultEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![gewu_multiverse::engine_api::SessionEnd::Succeeded];
            agent.last_result = Some("I fixed the flaky login test.".to_owned());
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
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
fn a_successful_runs_own_summary_is_beside_the_tick() {
    // Review B2: the result row used to be drawn only for refused/no-work, so a
    // green tick said nothing about what the run did.
    let mut board = Board::new(SuccessResultEngine(running_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("result: I fixed the flaky login test"),
        "the success has no result row:
{frame}"
    );
}

#[test]
fn the_run_legend_is_whole_at_the_floor() {
    // Review N4: the padded legend was cut to `◷ t…` on the 72-column card.
    let mut board = Board::new(StripEngine(running_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = flat(&draw(&mut board, 72, 18));
    assert!(
        frame.contains("⊘ refused"),
        "the legend lost the refusal word:\n{frame}"
    );
    assert!(
        frame.contains("◷ timed out"),
        "the legend is cut at the floor:\n{frame}"
    );
}

#[test]
fn the_warning_mark_is_in_the_legend_at_every_size() {
    // Correction 4: the new `⚠` mark must be in the legend at every promised size,
    // not only where there is room for the long word.
    let mut board = Board::new(StripEngine(running_engine(1)));
    press(&mut board, KeyCode::Enter);
    for (width, height) in SIZES {
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("⚠ cmd"),
            "{width}x{height}: the warning entry is not in the legend:\n{frame}"
        );
        assert!(
            frame.contains("⊘ refused") && frame.contains("◷ timed out"),
            "{width}x{height}: the legend lost an earlier word:\n{frame}"
        );
    }
}

#[test]
fn the_floor_card_states_are_whole() {
    // F-03: the compact card's one border line must not cut a state word.
    for (width, height) in [(72, 18), (80, 24)] {
        let mut board = Board::new(blocked_engine());
        let buffer = draw(&mut board, width, height);
        let frame = text(&buffer);
        assert!(
            frame.contains("needs instruction") || frame.contains("needs an instruction"),
            "{width}x{height}: the state word is cut:\n{frame}"
        );
    }
}

#[test]
fn the_home_card_keeps_its_few_words_at_72_and_80() {
    for (width, height) in [(72, 18), (80, 24)] {
        for count in [1, 4, 12] {
            let mut board = Board::new(endpoint_engine(count));
            let buffer = draw(&mut board, width, height);
            let frame = text(&buffer);
            assert!(
                frame.contains("agent-1") && frame.contains("gewu-agent-1"),
                "{width}x{height} with {count}: the name or login is missing:\n{frame}"
            );
            assert!(
                frame.contains("run "),
                "{width}x{height} with {count}: the run count is missing:\n{frame}"
            );
            assert!(
                !frame.contains("instruction"),
                "{width}x{height} with {count}: the rich detail is on the board card:\n{frame}"
            );
        }
    }
}

#[test]
fn one_four_and_twelve_agents_are_whole_at_72_and_80() {
    for (width, height) in [(72, 18), (80, 24)] {
        for count in [1, 4, 12] {
            let mut board = Board::new(endpoint_engine(count));
            check_frame(&mut board, width, height, &format!("count {count}"));
        }
    }
}

#[test]
fn the_floor_forgejo_page_keeps_every_fact_and_the_assignment_rule() {
    let mut board = Board::new(RichEngine(endpoint_engine(3)));
    press(&mut board, KeyCode::Char('f'));
    let frame = text(&draw(&mut board, 72, 18));
    let flat = flat(&draw(&mut board, 72, 18));
    assert!(
        flat.contains("only on issues assigned to them — assign an issue in Forgejo"),
        "the assignment rule is not whole at 72x18:\n{flat}"
    );
    for wanted in [
        "17.0.0",
        "organisation",
        "team",
        "team page",
        "/org/team/teams/agents",
        "only on issues assigned",
        "your login:",
    ] {
        assert!(
            frame.contains(wanted),
            "{wanted} missing at 72x18:\n{frame}"
        );
    }
}

#[test]
fn the_agent_count_screen_labels_itself_a_preview() {
    let mut board = picker_board();
    assert_eq!(board.screen(), Screen::Picker);
    let frame = text(&draw(&mut board, 80, 24));
    assert!(frame.contains("preview"), "{frame}");
    assert!(
        !frame.contains("● running"),
        "the preview shows a running server before create:\n{frame}"
    );
}

#[test]
fn the_selected_card_has_a_non_colour_mark() {
    let mut board = Board::new(endpoint_engine(3));
    let frame = text(&draw(&mut board, 120, 40));
    assert!(frame.contains("▸ agent-1"), "{frame}");
    press(&mut board, KeyCode::Right);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(frame.contains("▸ agent-2"), "{frame}");
    assert!(!frame.contains("▸ agent-1"), "{frame}");
}

#[test]
fn a_wide_window_uses_its_width_for_repositories_and_cards() {
    let mut board = Board::new(RichEngine(endpoint_engine(3)));
    let frame = text(&draw(&mut board, 200, 60));
    assert!(
        frame.contains("team/workspace") && frame.contains("team/website"),
        "a repository is still truncated at 200 columns:\n{frame}"
    );
    assert!(!frame.contains("team/workspa…"), "{frame}");
    let flat = flat(&draw(&mut board, 200, 60));
    assert!(flat.contains("your login: mei"), "{flat}");
}

#[test]
fn the_password_row_says_how_long_the_value_is_shown() {
    let mut board = Board::new(LongPasswordEngine {
        inner: endpoint_engine(3),
        revealed: false,
    });
    press(&mut board, KeyCode::Char('f'));
    let frame = text(&draw(&mut board, 80, 24));
    assert!(frame.contains("shown for 10 s"), "{frame}");
    assert!(frame.contains("old stops"), "{frame}");
    press(&mut board, KeyCode::Char('p'));
    let revealed = text(&draw(&mut board, 80, 24));
    assert!(revealed.contains("Gw-0123456789abcdef"), "{revealed}");
}

/// An engine whose newest run found no assigned issue: the Agent screen names the
/// rule instead of showing an empty work block (F2).
struct EmptyWorkEngine(FakeEngine);

impl EngineApi for EmptyWorkEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.last_work.clear();
            agent.no_assigned_work = true;
            agent.activity = None;
            agent.last_failure = None;
            agent.last_error = None;
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
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
fn the_agent_screen_names_the_assignment_rule_when_no_issue_is_assigned() {
    let mut board = Board::new(EmptyWorkEngine(endpoint_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(
        frame.contains("no issues assigned to gewu-agent-1 yet"),
        "{frame}"
    );
}

/// An engine whose newest clean run refused the instruction: the card must carry the
/// run's own summary beside a refused mark, not a green tick (F4).
struct RefusedEngine(FakeEngine);

impl EngineApi for RefusedEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Refused {
                    reason: "I could not delete team/library: the account has no permission."
                        .to_owned(),
                },
            ];
            agent.last_result =
                Some("I could not delete team/library: the account has no permission.".to_owned());
            agent.last_failure = None;
            agent.last_error = None;
            agent.activity = Some("opened pull request #4".to_owned());
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
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
fn a_refused_run_is_not_a_green_tick_and_carries_its_summary() {
    let mut board = Board::new(RefusedEngine(endpoint_engine(1)));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(frame.contains("⊘"), "the refused mark is missing:\n{frame}");
    assert!(
        frame.contains("⊘ refused"),
        "the legend lacks refused:\n{frame}"
    );
    assert!(frame.contains("result: I could not delete"), "{frame}");
    assert!(
        !frame.contains("last runs ✓ ✓"),
        "the refusal still reads as a green run:\n{frame}"
    );
}
