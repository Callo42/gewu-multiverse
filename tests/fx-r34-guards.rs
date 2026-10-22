//! fx-r34 guards: the one board picture at every size and count, and runs that
//! never outlive their supervisor.
//!
//! The layout guard renders 1, 3, 4, 7 and 12 agents at 72×18, 80×24, 100×30,
//! 120×40 and 200×60. It checks no two cards share a cell, every agent's name,
//! login, run count, state and last-work word are on the frame, the Forgejo box
//! keeps its login and repository count, a lone card has no stray connector, and
//! every box-drawing arm meets its neighbour.
//!
//! The process guards are live: they start the real `--run-agent` with a sleeping
//! step, kill the supervisor (and, in the second test, the product that started it)
//! with `SIGKILL`, and assert that a reopen reaps the shell and writes its
//! `run_stopped` record before opening.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use gewu_multiverse::board::{Board, BoardForm};
use gewu_multiverse::engine::{self, EngineOptions, TERMS_VERSION};
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command as BoardCommand, Desired, EndpointTest, EndpointView, EngineApi,
    ForgejoState, ForgejoView, Provisioning, SessionEnd, SessionState, UpgradeView,
};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{self, RunPlan};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

const SIZES: [(u16, u16); 5] = [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)];

/// Tests that spawn or signal real processes are serialized: a test's own exact
/// group cleanup must never race another test's child or a reused pid/group.
static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
const COUNTS: [usize; 5] = [1, 3, 4, 7, 12];

struct Fixed(BoardView);

impl EngineApi for Fixed {
    fn board(&mut self) -> BoardView {
        self.0.clone()
    }
    fn send(&mut self, _command: BoardCommand) -> Result<(), String> {
        Ok(())
    }
    fn tick(&mut self) {}
    fn instruction_path(&self, _agent: &str) -> Option<PathBuf> {
        None
    }
    fn session_files(&self, _agent: &str) -> Option<(PathBuf, PathBuf)> {
        None
    }
    fn shutdown(&mut self, _grace: Duration) {}
}

fn view(count: usize) -> BoardView {
    let agents = (0..count)
        .map(|index| {
            let number = index + 1;
            AgentView {
                name: format!("agent-{number}"),
                login: format!("gewu-agent-{number}"),
                provisioning: Provisioning::Ready,
                desired: Desired::Stopped,
                session: SessionState::Idle,
                sessions_since_start: 0,
                runs_total: number as u64 + 2,
                recent: vec![SessionEnd::Succeeded],
                instruction_chars: 12,
                instruction_first_line: Some("work the issue".to_owned()),
                instruction: Some("work the issue\n".to_owned()),
                instruction_lines: 1,
                activity: Some(format!("opened PR #{number}")),
                activity_at: Some(SystemTime::now() - Duration::from_secs(120)),
                last_error: None,
                last_result: Some(format!("did run {number}")),
                last_failure: None,
                leftover: None,
                current_tool: None,
                current_tool_seconds: 0,
                last_work: vec![format!("pull request #{}", 10 + index)],
                no_assigned_work: false,
                last_work_result: Some(format!("did run {number}")),
                last_result_is_work: true,
            }
        })
        .collect();
    BoardView {
        terms_acknowledged: true,
        terms: Vec::new(),
        setup_complete: true,
        forgejo: ForgejoView {
            state: ForgejoState::Running,
            url: "http://127.0.0.1:3000".to_owned(),
            version: "17.0.0".to_owned(),
            admin_login: "owner".to_owned(),
            organisation: "team".to_owned(),
            repositories: 2,
            repository_names: vec!["team/workspace".to_owned(), "team/website".to_owned()],
            events: vec!["team/scratch was deleted".to_owned()],
            admin_login_kept: false,
        },
        agents,
        endpoint: EndpointView {
            configured: true,
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key_present: true,
            test: EndpointTest::NotRun,
        },
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

/// The same fixed picture with a chosen newest work number, so a three-digit or
/// five-digit pull request can be checked at every size (F-08 review).
fn view_with_work(count: usize, base: u64) -> BoardView {
    let mut board_view = view(count);
    for (index, agent) in board_view.agents.iter_mut().enumerate() {
        agent.last_work = vec![format!("pull request #{}", base + index as u64)];
    }
    board_view
}

/// Whether the whole number word is on the text and not a prefix of a longer
/// number (`#100` must not satisfy `#10000`).
fn has_whole_word(text: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = text[from..].find(word) {
        let at = from + offset;
        let after = text[at + word.len()..].chars().next();
        if !after.is_some_and(|ch| ch.is_ascii_digit()) {
            return true;
        }
        from = at + 1;
    }
    false
}

fn draw(board: &mut Board, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    terminal.backend().buffer().clone()
}

fn frame_text(buffer: &Buffer) -> String {
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

fn rect_text(buffer: &Buffer, rect: ratatui::layout::Rect) -> String {
    let mut out = String::new();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

fn overlaps(a: ratatui::layout::Rect, b: ratatui::layout::Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

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

fn assert_connected(buffer: &Buffer, label: &str) {
    let area = buffer.area;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = buffer.cell((x, y)).expect("a cell");
            let symbol = cell.symbol();
            let Some(ch) = symbol.chars().next() else {
                continue;
            };
            let arms_here = arms(ch);
            if arms_here == 0 || symbol.chars().count() != 1 {
                continue;
            }
            for (arm, nx, ny) in [
                (L, x.checked_sub(1), Some(y)),
                (R, Some(x + 1), Some(y)),
                (U, Some(x), y.checked_sub(1)),
                (D, Some(x), Some(y + 1)),
            ] {
                if arms_here & arm == 0 {
                    continue;
                }
                let (Some(nx), Some(ny)) = (nx, ny) else {
                    panic!("{label}: {ch:?} at ({x},{y}) has an arm outside the window");
                };
                assert!(
                    nx < area.right() && ny < area.bottom(),
                    "{label}: arm outside"
                );
                let neighbour = buffer.cell((nx, ny)).expect("a neighbour");
                let neighbour_arms = neighbour.symbol().chars().next().map(arms).unwrap_or(0);
                assert!(
                    neighbour_arms & opposite(arm) != 0,
                    "{label}: {ch:?} at ({x},{y}) points into {:?} at ({nx},{ny})",
                    neighbour.symbol()
                );
            }
        }
    }
}

#[test]
fn one_picture_at_every_size_and_agent_count() {
    for (width, height) in SIZES {
        for count in COUNTS {
            let label = format!("{width}x{height} with {count} agents");
            let mut board = Board::new(Fixed(view(count)));
            let buffer = draw(&mut board, width, height);
            let frame = frame_text(&buffer);
            let layout = board.layout();

            // No two cards (or list lines) share a cell.
            for (index, card) in layout.cards.iter().enumerate() {
                for (other, slot) in layout.cards.iter().enumerate().skip(index + 1) {
                    assert!(
                        !overlaps(card.rect, slot.rect),
                        "{label}: cards {other} and {index} overlap"
                    );
                }
            }
            for (index, line) in layout.listed.iter().enumerate() {
                for (other, slot) in layout.listed.iter().enumerate().skip(index + 1) {
                    assert!(
                        !overlaps(line.rect, slot.rect),
                        "{label}: list lines {other} and {index} overlap"
                    );
                }
            }

            // The Forgejo box keeps the login and the repository count at every size.
            assert!(frame.contains("your login: owner"), "{label}:\n{frame}");
            assert!(frame.contains("2 repositories"), "{label}:\n{frame}");
            assert!(
                frame.contains("team/scratch was deleted"),
                "{label}:\n{frame}"
            );

            // Every agent's facts are on the frame.
            let form = if layout.cards.is_empty() {
                BoardForm::List
            } else {
                BoardForm::Cards
            };
            if width == 80 && count == 7 {
                assert_eq!(
                    form,
                    BoardForm::Cards,
                    "{label}: 80x24 with seven agents is not cards"
                );
            }
            assert_eq!(layout.cards.len() + layout.listed.len(), count, "{label}");
            for slot in layout.cards.iter().chain(layout.listed.iter()) {
                let agent = &view(count).agents[slot.agent];
                let text = rect_text(&buffer, slot.rect);
                assert!(
                    text.contains(&agent.name),
                    "{label}: {}:\n{text}",
                    agent.name
                );
                assert!(
                    text.contains(&agent.login),
                    "{label}: {}:\n{text}",
                    agent.login
                );
                assert!(
                    text.contains(&format!("{}", agent.runs_total)),
                    "{label}: run count missing for {}:\n{text}",
                    agent.name
                );
                let word = format!("#{}", 10 + slot.agent);
                assert!(
                    text.contains(&word),
                    "{label}: last-work word {word} missing for {}:\n{text}\n{frame}",
                    agent.name
                );
                if form == BoardForm::Cards {
                    assert!(
                        text.contains("stopped"),
                        "{label}: state word missing for {}:\n{text}",
                        agent.name
                    );
                    assert!(
                        !text.contains('…'),
                        "{label}: card {} is truncated:\n{text}",
                        agent.name
                    );
                } else {
                    assert!(text.contains('○'), "{label}: state mark missing:\n{text}");
                }
            }
            if form == BoardForm::List {
                assert!(
                    frame.contains("● working"),
                    "{label}: no state legend:\n{frame}"
                );
            }

            // A lone card has no stray connector: no bus, just the trunk.
            if count == 1 {
                assert_eq!(layout.cards.len(), 1, "{label}");
                assert_eq!(layout.wire_cells.len(), 1, "{label}");
                assert!(
                    !frame.contains('├') && !frame.contains('┤'),
                    "{label}:\n{frame}"
                );
                let (x, y) = layout.wire_cells[0];
                assert_eq!(
                    buffer.cell((x, y)).expect("wire").symbol(),
                    "│",
                    "{label}: the lone card has a bus stub"
                );
            }

            // Every box-drawing arm meets its neighbour; each card has a wire.
            assert_connected(&buffer, &label);
            for slot in &layout.cards {
                let junction = (slot.rect.x..slot.rect.right())
                    .find(|x| buffer.cell((*x, slot.rect.y)).expect("top border").symbol() == "┴")
                    .unwrap_or_else(|| panic!("{label}: no wire junction on a card"));
                assert!(
                    layout.wire_cells.contains(&(junction, slot.rect.y - 1)),
                    "{label}: the wire is not recorded"
                );
            }
        }
    }
}

// ---- one picture at every size: the work number ---------------------------

#[test]
fn the_last_work_number_is_never_truncated_at_any_size() {
    // F-08 review: `PR #100` used to become `PR#10`, and `#10000` `#1000`.
    // Every size and count keeps the whole number, three digits and five.
    for base in [100_u64, 10_000_u64] {
        for (width, height) in SIZES {
            for count in COUNTS {
                let label = format!("{base} @ {width}x{height} with {count} agents");
                let mut board = Board::new(Fixed(view_with_work(count, base)));
                let buffer = draw(&mut board, width, height);
                let layout = board.layout();
                assert_eq!(
                    layout.cards.len() + layout.listed.len(),
                    count,
                    "{label}: not every agent is on the frame"
                );
                for slot in layout.cards.iter().chain(layout.listed.iter()) {
                    let expected = format!("#{}", base + slot.agent as u64);
                    let text = rect_text(&buffer, slot.rect);
                    assert!(
                        has_whole_word(&text, &expected),
                        "{label}: {expected} is missing or cut for agent {}:\n{text}",
                        slot.agent
                    );
                }
            }
        }
    }
}

#[test]
fn a_failed_run_keeps_http_status_and_the_connector_never_crosses_it() {
    // F-14 review: the failure word is cut on whole words, status first, and the
    // next row's connector is placed after the label, never through it.
    for (width, height) in [(72_u16, 18_u16), (80, 24)] {
        for count in [3_usize, 7] {
            let label = format!("{width}x{height} with {count} agents");
            let mut board_view = view(count);
            board_view.agents[0].session = SessionState::Idle;
            board_view.agents[0].last_error = Some("the model answered HTTP 500".to_owned());
            board_view.agents[0].recent = vec![SessionEnd::Failed {
                reason: "the model call failed (server_error): the model answered HTTP 500"
                    .to_owned(),
            }];
            let mut board = Board::new(Fixed(board_view));
            let buffer = draw(&mut board, width, height);
            let frame = frame_text(&buffer);
            let layout = board.layout();
            let card = layout.cards.iter().find(|slot| slot.agent == 0);
            let slot = card
                .or_else(|| layout.listed.iter().find(|slot| slot.agent == 0))
                .unwrap_or_else(|| panic!("{label}: no slot for the failed agent"));
            let text = rect_text(&buffer, slot.rect);
            if card.is_some() {
                assert!(
                    text.contains("HTTP 500"),
                    "{label}: the failure word loses its HTTP status:\n{text}"
                );
            } else {
                // The list form has one word, not a sentence (W67-3); the status
                // code itself is what must survive (F-14).
                assert!(
                    text.contains("500") && text.contains('✗'),
                    "{label}: the list loses the failure mark or status:\n{text}"
                );
            }
            assert!(
                !text.contains("5┬0") && !text.contains("5┼0") && !text.contains("5│0"),
                "{label}: a connector is drawn through the status:\n{text}"
            );
            assert!(
                text.contains("stopped"),
                "{label}: the state word is gone:\n{text}"
            );
            assert!(
                text.contains("gewu-agent-1") && text.contains("run 3"),
                "{label}: the card loses its login or run count:\n{text}"
            );
            if card.is_some() {
                let bottom = slot.rect.bottom() - 1;
                let row: String = (slot.rect.x..slot.rect.right())
                    .map(|x| buffer.cell((x, bottom)).expect("bottom border").symbol())
                    .collect();
                if let Some(junction) = row.find('┬') {
                    let status = row.find("500").expect("the status is on the bottom row");
                    assert!(
                        junction > status + 3,
                        "{label}: the connector is drawn through the failure word:\n{row}"
                    );
                }
            }
            if count == 3 && width == 72 {
                assert!(
                    frame.contains("HTTP 500 from the model"),
                    "{label}: the compact card cuts the failure sentence:\n{frame}"
                );
            }
        }
    }
}

#[test]
fn the_quit_warning_uses_its_rows_and_keeps_the_consequences() {
    // F-13 review: at 72x18 and 80x24 with 7 and 12 running agents the warning
    // must name what stops, say they start again when you reopen, and keep `q again
    // quits`; the geometry's footer rows are the room.
    for (width, height) in [(72_u16, 18_u16), (80, 24)] {
        for count in [7_usize, 12] {
            let label = format!("{width}x{height} with {count} running agents");
            let mut board_view = view(count);
            for agent in &mut board_view.agents {
                agent.desired = Desired::Started;
                agent.session = SessionState::Running {
                    number: 1,
                    since: SystemTime::now(),
                };
            }
            let mut board = Board::new(Fixed(board_view));
            board.on_key(KeyEvent::from(KeyCode::Char('q')));
            let frame = frame_text(&draw(&mut board, width, height));
            assert!(frame.contains("quit stops"), "{label}:\n{frame}");
            // The sentence is wrapped, never cut: all of its words are on the
            // frame. The confirmation is kept whole on one line.
            assert!(
                frame.contains("start") && frame.contains("again") && frame.contains("on reopen"),
                "{label}: the reopen sentence is cut:\n{frame}"
            );
            assert!(
                frame.contains("q again quits"),
                "{label}: the confirmation is cut:\n{frame}"
            );
            assert!(frame.contains("Forgejo"), "{label}:\n{frame}");
            for number in 1..=count {
                let name = format!("agent-{number}");
                assert!(
                    frame.contains(&name),
                    "{label}: {name} is not named:\n{frame}"
                );
            }
        }
    }
}

/// One cell of the review-2 quit-warning matrix: render the confirming-quit
/// footer and check that the warning is whole and the board above it is intact.
fn assert_quit_warning_cell(width: u16, height: u16, count: usize, event: bool, status: bool) {
    let label = format!("{width}x{height}, {count} running agents, event={event}, status={status}");
    let mut board_view = view(count);
    for agent in &mut board_view.agents {
        agent.desired = Desired::Started;
        agent.session = SessionState::Running {
            number: 1,
            since: SystemTime::now(),
        };
    }
    if !event {
        board_view.forgejo.events.clear();
    }
    if status {
        board_view.notice = Some("copied repository URL".to_owned());
    }
    let mut board = Board::new(Fixed(board_view));
    board.on_key(KeyEvent::from(KeyCode::Char('q')));
    let frame = frame_text(&draw(&mut board, width, height));
    // Drop the frame's borders before flattening, so a wrapped sentence can be
    // checked as words rather than as pixels.
    let flat = frame
        .chars()
        .map(|ch| {
            if matches!(ch, '│' | '╭' | '╮' | '╰' | '╯' | '─') {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // What stops: every running agent, and the Forgejo.
    assert!(flat.contains("quit stops"), "{label}:\n{frame}");
    for number in 1..=count {
        let name = format!("agent-{number}");
        assert!(
            has_whole_word(&flat, &name),
            "{label}: {name} is not named whole:\n{frame}"
        );
    }
    assert!(
        flat.contains("Forgejo; they start again on reopen — q again quits"),
        "{label}: the consequence, reopen sentence or confirmation is cut:\n{frame}"
    );
    assert!(
        flat.contains("q again quits"),
        "{label}: the confirmation is cut:\n{frame}"
    );

    // The picture above gives up rows, never the warning: every drawn card or
    // list row stays above the reserved footer, and no two share a cell.
    let footer = board.layout().footer_rows;
    assert!(footer >= 1, "{label}: no footer rows");
    let content_bottom = height - 1 - footer;
    let lines: Vec<&str> = frame.lines().collect();
    let warning_top = lines
        .iter()
        .position(|line| line.contains("quit stops"))
        .unwrap_or_else(|| panic!("{label}: no warning line:\n{frame}"));
    let warning_bottom = lines
        .iter()
        .rposition(|line| line.contains("quits"))
        .unwrap_or_else(|| panic!("{label}: no confirmation line:\n{frame}"));
    assert!(
        warning_top >= content_bottom as usize,
        "{label}: the warning starts inside the board at row {warning_top} \
         (content ends at {content_bottom}):\n{frame}"
    );
    assert!(
        warning_bottom < height as usize - 1,
        "{label}: the warning is drawn past the footer frame:\n{frame}"
    );
    let slots: Vec<ratatui::layout::Rect> = board
        .layout()
        .cards
        .iter()
        .chain(board.layout().listed.iter())
        .map(|slot| slot.rect)
        .collect();
    assert!(!slots.is_empty(), "{label}: no agent is drawn");
    for rect in &slots {
        assert!(
            rect.y >= 1 && rect.bottom() <= content_bottom,
            "{label}: a card at {rect:?} reaches into the footer \
             (content bottom {content_bottom}):\n{frame}"
        );
    }
    for (index, one) in slots.iter().enumerate() {
        for two in &slots[index + 1..] {
            assert!(
                one.x >= two.right()
                    || two.x >= one.right()
                    || one.y >= two.bottom()
                    || two.y >= one.bottom(),
                "{label}: cards {one:?} and {two:?} share a cell:\n{frame}"
            );
        }
    }
}

/// The review-2 blocker: the quit warning's lines are part of the footer
/// geometry, so a 12-agent warning is never clipped for want of rows. The full
/// matrix is 1, 4, 7 and 12 running agents at every supported size, with and
/// without a repository-event row and with and without a status sentence.
#[test]
fn the_quit_warning_is_whole_in_every_footer_state() {
    for (width, height) in SIZES {
        for count in [1_usize, 4, 7, 12] {
            for event in [false, true] {
                for status in [false, true] {
                    assert_quit_warning_cell(width, height, count, event, status);
                }
            }
        }
    }
}

/// The two cells the reviewer saw cut: twelve running agents, no repository
/// event row, no status sentence, at 72x18 and 80x24. Each is its own guard so
/// that both fail at 5294397 rather than the first masking the second.
#[test]
fn the_12_agent_quit_warning_at_72x18_needs_no_event_row() {
    assert_quit_warning_cell(72, 18, 12, false, false);
}

#[test]
fn the_12_agent_quit_warning_at_80x24_needs_no_event_row() {
    assert_quit_warning_cell(80, 24, 12, false, false);
}

#[test]
fn the_reaper_reads_no_process_environment() {
    // The review's blocker 5: the reap path must not scan other processes'
    // environments. Group membership comes from `stat`; trust comes from the
    // run's own records.
    let source = include_str!("../src/run.rs");
    assert!(
        !source.contains("/environ") && !source.contains("\"environ\""),
        "src/run.rs opens a process environment during reap"
    );
}

// ---- runs never outlive their supervisor ---------------------------------

fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "gewu-fx-r34-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("scratch");
    path
}

fn write_run(layout: &Layout, agent: &str, run_id: &str) -> PathBuf {
    layout.ensure_root().expect("config root");
    let paths = layout.ensure_agent(agent).expect("agent paths");
    fs::create_dir_all(paths.workspace()).expect("workspace");
    fs::create_dir_all(paths.skills_dir()).expect("skills");
    fs::write(paths.instruction(), "wait for a while\n").expect("instruction");
    if !layout.credentials_file().is_file() {
        gewu_multiverse::credentials::record_secret(
            &layout.credentials_file(),
            None,
            "model_key",
            "fx-r34-synthetic-key",
        )
        .expect("model key");
    }
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("run dir");
    let plan = RunPlan {
        run_id: run_id.to_owned(),
        agent: agent.to_owned(),
        workspace: paths.workspace().to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "demo".to_owned(),
        credentials: layout.credentials_file(),
        max_turns: 4,
        steps: vec!["sleep 60".to_owned()],
        timeout_secs: 120,
        confinement: "none".to_owned(),
        hide: Vec::new(),
    };
    plan.write(&run_dir).expect("plan");
    run_dir
}

fn process_cmdline(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
        .unwrap_or_default()
}

fn process_group(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// The live members of a group, from `/proc/<pid>/stat` only. The guard itself
/// reads no process environment either.
fn group_members(pgid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if process_group(pid) == Some(pgid) {
            found.push(pid);
        }
    }
    found
}

fn shell_members(pgid: u32) -> Vec<u32> {
    group_members(pgid)
        .into_iter()
        .filter(|pid| {
            let cmd = process_cmdline(*pid);
            cmd.contains("sleep 60") && !cmd.contains("--run-agent")
        })
        .collect()
}

fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check()
}

fn write_records(run_dir: &Path, pid: u32) {
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("pid record");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("pgid record");
}

fn spawn_supervisor(run_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("the supervisor starts")
}

#[test]
fn a_supervisor_waiting_for_the_model_is_reaped_before_its_first_shell() {
    let _process_test = process_test_lock();
    let root = scratch("supervisor-before-shell");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000030-0001");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("model fixture");
    listener.set_nonblocking(true).expect("nonblocking accept");
    let mut plan = RunPlan::read(&run_dir).expect("plan");
    plan.steps.clear();
    plan.endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    plan.write(&run_dir).expect("model plan");
    let mut supervisor = spawn_supervisor(&run_dir);
    write_records(&run_dir, supervisor.id());
    let mut request = None;
    let waiting = wait_until(Duration::from_secs(5), || {
        if let Ok((stream, _)) = listener.accept() {
            request = Some(stream);
        }
        request.is_some()
    });
    let no_capability = !run_dir.join("shell.capability").exists();
    let result = run::reap_leftover_run(&run_dir, Duration::from_millis(150));
    let stopped = wait_until(Duration::from_secs(2), || {
        supervisor.try_wait().is_ok_and(|status| status.is_some())
    });
    if !stopped {
        stop_unrelated(supervisor);
    }
    drop(request);
    assert!(
        waiting,
        "the actual supervisor never reached the model fixture"
    );
    assert!(
        no_capability,
        "this control must precede the first shell capability"
    );
    assert!(result.expect("the actual supervisor is owned"));
    assert!(stopped, "the model-waiting supervisor survived cleanup");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_killed_supervisors_shell_is_reaped_at_the_next_start() {
    let _process_test = process_test_lock();
    let root = scratch("killed-supervisor");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_id = "run-1700000000001-0001";
    let run_dir = write_run(&layout, "agent-1", run_id);
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    write_records(&run_dir, pgid);
    assert!(
        wait_until(Duration::from_secs(10), || {
            !shell_members(pgid).is_empty()
        }),
        "the sleeping shell never started"
    );
    // Kill only the supervisor, not its group: the shell stays behind.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the supervisor");
    let _ = supervisor.wait();
    assert!(
        !shell_members(pgid).is_empty(),
        "the killed supervisor's shell did not survive; the test cannot prove the reap"
    );
    let reaped = run::reap_leftover_run(&run_dir, Duration::from_secs(3)).expect("the reap");
    assert!(reaped, "the left-over group was not reported as reaped");
    assert!(
        shell_members(pgid).is_empty(),
        "a shell survived the reap: {:?}",
        shell_members(pgid)
    );
    assert!(
        run_dir.join("run-stopped.json").is_file(),
        "no run_stopped record"
    );
    let terminal =
        gewu_multiverse::transcript::terminal_record(&run_dir).expect("a terminal record");
    assert!(
        format!("{terminal:?}").contains("Stopped") || format!("{terminal:?}").contains("stopped"),
        "the terminal record does not say stopped: {terminal:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_killed_products_left_over_shell_is_reaped_on_reopen() {
    let _process_test = process_test_lock();
    let root = scratch("killed-product");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_id = "run-1700000000002-0001";
    let run_dir = write_run(&layout, "agent-1", run_id);
    let mut helper = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "fx_r34_product_parent_helper", "--nocapture"])
        .env("GEWU_FX_R34_HELPER_RUN", &run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts");
    let helper_pid = helper.id();
    // Wait for the run-agent child the helper started.
    assert!(
        wait_until(Duration::from_secs(10), || {
            let mut found = false;
            if let Ok(entries) = fs::read_dir("/proc") {
                for entry in entries.flatten() {
                    let Some(pid) = entry
                        .file_name()
                        .to_str()
                        .and_then(|text| text.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    let cmdline = process_cmdline(pid);
                    if cmdline.contains("--run-agent")
                        && cmdline.contains(run_dir.to_str().unwrap_or(""))
                    {
                        write_records(&run_dir, pid);
                        found = true;
                    }
                }
            }
            found
                && !shell_members(
                    fs::read_to_string(run_dir.join("session.pgid"))
                        .ok()
                        .and_then(|text| text.trim().parse().ok())
                        .unwrap_or(0),
                )
                .is_empty()
        }),
        "the helper's run never reached its shell"
    );
    let pgid: u32 = fs::read_to_string(run_dir.join("session.pgid"))
        .expect("pgid")
        .trim()
        .parse()
        .expect("number");
    // Kill the product (the parent process), not the supervisor.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(helper_pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the product");
    let _ = helper.wait();
    assert!(
        wait_until(Duration::from_secs(5), || {
            process_cmdline(pgid).is_empty()
        }),
        "the supervisor did not end with its product"
    );
    assert!(
        !shell_members(pgid).is_empty(),
        "the product's shell did not survive; the test cannot prove the reopen reap"
    );
    // Reopen the engine: recovery runs before it opens.
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let _ = engine.board();
    assert!(
        shell_members(pgid).is_empty(),
        "the reopen left the product's shell running"
    );
    assert!(
        run_dir.join("run-stopped.json").is_file(),
        "no run_stopped record"
    );
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// Spawn an unrelated process in its own group, so a stale record can be shown
/// never to own it. The process is task-owned and is stopped exactly in cleanup.
fn spawn_unrelated_group(tag: &str) -> Child {
    let _ = tag;
    Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the unrelated group starts")
}

fn stop_unrelated(mut child: Child) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.wait();
}

fn unrelated_alive(pid: u32) -> bool {
    process_cmdline(pid).contains("sleep 30")
}

fn stat_start(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("stat");
    let rest = stat.rsplit_once(')').expect("comm").1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next();
    let _ppid = fields.next();
    let _pgrp = fields.next();
    for _ in 6..=21 {
        let _ = fields.next();
    }
    fields.next().expect("start").parse().expect("start number")
}

/// A stale `shell.pid`/`shell.pgid` naming a live unrelated group must not be
/// believed: stored numbers are not identity (review-3 blocker 1).
#[test]
fn a_stale_shell_record_never_owns_an_unrelated_group() {
    let _process_test = process_test_lock();
    let root = scratch("stale-shell");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000010-0001");
    let child = spawn_unrelated_group("stale-shell");
    let pid = child.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated process did not start"
    );
    fs::write(run_dir.join("shell.pid"), pid.to_string()).expect("shell pid");
    fs::write(run_dir.join("shell.pgid"), pid.to_string()).expect("shell pgid");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("a live unverified group must be reported, not discarded");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unverified group was not named: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated process group was killed from a stale record"
    );
    assert!(
        run_dir.join("shell.pgid").is_file(),
        "the unverified group's records were silently discarded"
    );
    assert!(
        !run_dir.join("run-stopped.json").is_file(),
        "a stale record got a false run_stopped claim"
    );
    stop_unrelated(child);
    let _ = fs::remove_dir_all(&root);
}

/// A reused supervisor pid, with no shell identity and no command line naming the
/// run, is a number, not a process of this run.
#[test]
fn a_reused_supervisor_pid_is_never_identity() {
    let _process_test = process_test_lock();
    let root = scratch("stale-session");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000011-0001");
    let child = spawn_unrelated_group("stale-session");
    let pid = child.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated process did not start"
    );
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("session pid");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("session pgid");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("a live unverified group must be reported, not discarded");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unverified group was not named: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated process group was killed from a reused pid"
    );
    assert!(
        run_dir.join("session.pgid").is_file(),
        "the unverified group's records were silently discarded"
    );
    assert!(
        !run_dir.join("run-stopped.json").is_file(),
        "a reused pid got a false run_stopped claim"
    );
    stop_unrelated(child);
    let _ = fs::remove_dir_all(&root);
}

/// The shell record's identity must match the live process's boot and kernel start
/// time. A record with the right numbers but the wrong start time is not ours.
#[test]
fn an_unverified_shell_identity_never_signals_the_group() {
    let _process_test = process_test_lock();
    let root = scratch("stale-identity");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000012-0001");
    let child = spawn_unrelated_group("stale-identity");
    let pid = child.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated process did not start"
    );
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot id")
        .trim()
        .to_owned();
    let wrong_start = stat_start(pid).saturating_add(1);
    fs::write(run_dir.join("shell.pid"), pid.to_string()).expect("shell pid");
    fs::write(run_dir.join("shell.pgid"), pid.to_string()).expect("shell pgid");
    fs::write(
        run_dir.join("shell.identity"),
        format!("{pid} {pid} {boot} {wrong_start}\n"),
    )
    .expect("shell identity");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("a live unverified group must be reported, not discarded");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unverified group was not named: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated process group was killed from a mismatched identity record"
    );
    assert!(
        run_dir.join("shell.identity").is_file(),
        "the unverified group's records were silently discarded"
    );
    assert!(
        !run_dir.join("run-stopped.json").is_file(),
        "a mismatched identity got a false run_stopped claim"
    );
    stop_unrelated(child);
    let _ = fs::remove_dir_all(&root);
}

/// A failed cleanup must reach the card, keep its reason, and stop a replacement
/// run. Start retries the cleanup; once the obstruction is gone, it starts.
#[test]
fn a_failed_cleanup_reaches_the_card_and_blocks_a_replacement_run() {
    let _process_test = process_test_lock();
    let root = scratch("leftover-card");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000013-0001");
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    write_records(&run_dir, pgid);
    assert!(
        wait_until(Duration::from_secs(10), || {
            run_dir.join("shell.pgid").is_file() && !shell_members(pgid).is_empty()
        }),
        "the shell or its run-directory record never appeared"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the supervisor");
    let _ = supervisor.wait();
    assert!(
        !shell_members(pgid).is_empty(),
        "the shell did not survive; the test cannot prove the failed cleanup"
    );
    // The group is reaped, but the run-directory record write cannot happen: a
    // cleanup failure a person can see.
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o555))
        .expect("make the run directory read-only");
    let mut settings = gewu_multiverse::engine::settings::Settings {
        terms_acknowledged: true,
        terms_version: TERMS_VERSION,
        ..Default::default()
    };
    settings.endpoint.url = "http://127.0.0.1:9/v1/chat/completions".to_owned();
    settings.endpoint.model = "demo".to_owned();
    settings
        .add_agent("agent-1", Desired::Stopped)
        .expect("an agent");
    settings
        .save(&gewu_multiverse::engine::settings::path_in(&root))
        .expect("settings");
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let board = engine.board();
    let reason = board.agents[0]
        .leftover
        .clone()
        .expect("the failed cleanup is not on the card");
    assert!(
        reason.contains("cannot be written"),
        "the card reason is not the cleanup failure: {reason}"
    );
    assert!(
        matches!(&board.agents[0].session, SessionState::Blocked { .. }),
        "a failed cleanup did not block the agent: {:?}",
        board.agents[0].session
    );
    assert_eq!(
        board.agents[0].last_failure.as_deref(),
        Some(reason.as_str()),
        "the cleanup reason is not on the engine view"
    );
    let error = engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect_err("Start must not start a replacement while the cleanup fails");
    assert!(
        error.contains("cannot be written"),
        "Start did not report the cleanup failure: {error}"
    );
    assert!(
        engine.board().agents[0].leftover.is_some(),
        "Start cleared the leftover although its retry failed"
    );
    // The truthful route: repair the obstruction, Start retries, and only then is
    // the agent allowed to start.
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
        .expect("make the run directory writable again");
    engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start retries the cleanup after it is repaired");
    let after = engine.board();
    assert!(
        after.agents[0].leftover.is_none(),
        "the repaired cleanup did not clear the leftover"
    );
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// The anonymous memfd capability is live identity even before
/// `shell.pid`/`shell.identity`: the shell inherits the descriptor, and record
/// numbers alone do not make a group ours (review-3b).
#[test]
fn an_inherited_capability_finds_the_group_before_its_pid_record() {
    let _process_test = process_test_lock();
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let root = scratch("group-capability");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000014-0001");
    let fd =
        nix::sys::memfd::memfd_create("gewu-test-capability", nix::sys::memfd::MFdFlags::empty())
            .expect("memfd");
    let mut capability = fs::File::from(fd);
    let token = [0x5a_u8; 16];
    capability.write_all(&token).expect("token");
    capability.seek(SeekFrom::Start(0)).expect("seek");
    let metadata = capability.metadata().expect("metadata");
    fs::write(
        run_dir.join("shell.capability"),
        format!(
            "{} {} {}\n",
            metadata.dev(),
            metadata.ino(),
            token
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    )
    .expect("capability record");
    let child = spawn_unrelated_group("group-capability");
    let pid = child.id();
    drop(capability);
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the capability-holding process did not start"
    );
    // No shell record yet; only the inherited capability and the group number.
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("session pid");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("session pgid");
    let reaped = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect("the capability group is reaped");
    assert!(
        reaped,
        "the inherited capability was not accepted as identity"
    );
    assert!(
        !unrelated_alive(pid),
        "the capability-holding group was not stopped"
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let mut child = child;
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
}

/// The reviewer's unrelated-open probe: a live process that merely opens the old
/// named `shell.marker` path is not this run's owner and must survive.
#[test]
fn an_unrelated_process_opening_the_named_marker_is_not_owned() {
    let _process_test = process_test_lock();
    let root = scratch("opened-marker");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000018-0001");
    let marker = run_dir.join("shell.marker");
    fs::write(&marker, b"").expect("marker");
    let opener = Command::new("/bin/bash")
        .arg("-c")
        .arg(format!("exec 3<{}; sleep 30", marker.display()))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the unrelated opener starts");
    let pid = opener.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated opener did not start"
    );
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("session pid");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("session pgid");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("a merely opened marker path must not be ownership");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unowned group was not reported: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated process that opened the marker path was killed"
    );
    stop_unrelated(opener);
    let _ = fs::remove_dir_all(&root);
}

/// The capability *record* is a description, not ownership: an unrelated process
/// that opens the record path must not be killed (review-3b).
#[test]
fn an_unrelated_process_opening_the_capability_record_is_not_owned() {
    let _process_test = process_test_lock();
    let root = scratch("opened-capability");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000024-0001");
    let record = run_dir.join("shell.capability");
    fs::write(
        &record,
        b"1 2 00112233445566778899aabbccddeeff
",
    )
    .expect("record");
    let opener = Command::new("/bin/bash")
        .arg("-c")
        .arg(format!("exec 3<{}; sleep 30", record.display()))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the unrelated opener starts");
    let pid = opener.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated opener did not start"
    );
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("session pid");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("session pgid");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("opening the record file must not be ownership");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unowned group was not reported: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated process that opened the capability record was killed"
    );
    stop_unrelated(opener);
    let _ = fs::remove_dir_all(&root);
}

/// If the inherited marker cannot be prepared, the shell must not run at all:
/// there must be no unprotected window (review-3).
#[test]
fn a_capability_preparation_failure_stops_the_run() {
    let _process_test = process_test_lock();
    let root = scratch("marker-failure");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000017-0001");
    // A directory in the marker's place makes the marker file impossible to open
    // while leaving the rest of the run directory writable.
    fs::create_dir(run_dir.join("shell.capability")).expect("capability obstruction");
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    // The supervisor may spend a moment failing the step; what matters is that no
    // shell exists in the window, not that the model loop has already exited.
    let _ = wait_until(Duration::from_secs(2), || {
        !shell_members(pgid).is_empty()
            || fs::read_to_string(run_dir.join("run.log"))
                .is_ok_and(|log| log.contains("capability") || log.contains("cannot be written"))
    });
    assert!(
        shell_members(pgid).is_empty(),
        "a shell ran without its recovery capability: {:?}",
        shell_members(pgid)
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = supervisor.wait();
    fs::remove_dir_all(run_dir.join("shell.capability")).expect("obstruction");
    let _ = fs::remove_dir_all(&root);
}

fn mark_run_started(run_dir: &Path) {
    let mut transcript =
        gewu_multiverse::transcript::Transcript::open(run_dir).expect("the transcript opens");
    transcript
        .append(&gewu_multiverse::transcript::Record::run_started(
            "run-test",
            "agent-1",
            std::process::id(),
            "/tmp/workspace",
        ))
        .expect("the started record");
}

fn settings_with_agent(root: &Path) {
    let mut settings = gewu_multiverse::engine::settings::Settings {
        terms_acknowledged: true,
        terms_version: TERMS_VERSION,
        ..Default::default()
    };
    settings.endpoint.url = "http://127.0.0.1:9/v1/chat/completions".to_owned();
    settings.endpoint.model = "demo".to_owned();
    settings
        .add_agent("agent-1", Desired::Stopped)
        .expect("an agent");
    settings
        .save(&gewu_multiverse::engine::settings::path_in(root))
        .expect("settings");
}

/// A started run whose process records are gone is not cleared into a
/// replacement: it reaches the card and blocks Start until the lost identity is
/// honestly resolved (review-3b).
#[test]
fn a_started_run_with_lost_process_records_blocks_start() {
    let root = scratch("lost-records");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let _run_dir = write_run(&layout, "agent-1", "run-1700000000019-0001");
    mark_run_started(&_run_dir);
    settings_with_agent(&root);
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let board = engine.board();
    let reason = board.agents[0]
        .leftover
        .clone()
        .expect("the lost process record is not on the card");
    assert!(
        reason.contains("records are lost") || reason.contains("cannot be verified"),
        "the card reason is not the lost record: {reason}"
    );
    let error = engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect_err("Start must not replace a run with lost process records");
    assert!(
        error.contains("records are lost") || error.contains("cannot be verified"),
        "Start did not report the lost record: {error}"
    );
    assert_eq!(engine.board().agents[0].runs_total, 1);
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// A repaired unreadable agent directory is re-enumerated on Start; it is not
/// handed to the single-run reaper (review-3b).
#[test]
fn a_repaired_agent_directory_is_rescanned_before_start() {
    let _process_test = process_test_lock();
    let root = scratch("agent-retry");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000020-0001");
    settings_with_agent(&root);
    let child = spawn_unrelated_group("agent-retry");
    let pid = child.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the run's old process did not start"
    );
    fs::write(run_dir.join("shell.pid"), pid.to_string()).expect("shell pid");
    fs::write(run_dir.join("shell.pgid"), pid.to_string()).expect("shell pgid");
    let agent_dir = run_dir.parent().expect("agent dir");
    fs::set_permissions(agent_dir, fs::Permissions::from_mode(0o400))
        .expect("make the agent directory readable but not searchable");
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let reason = engine.board().agents[0]
        .leftover
        .clone()
        .expect("the partial agent directory is not on the card");
    assert!(
        reason.contains("searched") || reason.contains("cannot be read"),
        "the card reason is not the agent permission: {reason}"
    );
    fs::set_permissions(agent_dir, fs::Permissions::from_mode(0o700))
        .expect("repair the agent directory");
    let error = engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect_err("Start must rescan the repaired agent directory and still block");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "Start did not rescan the agent directory: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "the old group was touched instead of reported"
    );
    stop_unrelated(child);
    let _ = fs::remove_dir_all(&root);
}

/// Readable but not searchable run directories and denied record metadata are
/// failures, not absent files (review-3b).
#[test]
fn a_readable_but_unsearchable_run_directory_reaches_the_card() {
    let root = scratch("partial-perm");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000021-0001");
    mark_run_started(&run_dir);
    settings_with_agent(&root);
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o400))
        .expect("readable, not searchable");
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let board = engine.board();
    let reason = board.agents[0]
        .leftover
        .clone()
        .expect("the partial permission is not on the card");
    assert!(
        reason.contains("cannot be searched") || reason.contains("cannot be read"),
        "the card reason is not the partial permission: {reason}"
    );
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).expect("repair permissions");
    // The started run still has no verifiable process identity, so the repaired
    // directory is rescanned and remains blocked rather than starting a replacement.
    assert!(
        engine
            .send(BoardCommand::Start {
                name: "agent-1".to_owned(),
            })
            .is_err(),
        "Start admitted a replacement although the started run has no records"
    );
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// A live group with no selected target is not "gone": a surviving unverified
/// member is a visible failed cleanup (review-3b).
#[test]
fn an_empty_target_list_with_a_live_member_is_an_error() {
    let _process_test = process_test_lock();
    let root = scratch("empty-targets");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000022-0001");
    let child = spawn_unrelated_group("empty-targets");
    let pgid = child.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pgid)),
        "the unrelated process did not start"
    );
    let error = run::reap_process_group(pgid, &run_dir, Duration::from_millis(100))
        .expect_err("a live member must not be reported as gone");
    assert!(
        error.contains("cannot be verified") || error.contains("surviving member"),
        "the live member was not named: {error}"
    );
    assert!(unrelated_alive(pgid), "the unrelated process was touched");
    stop_unrelated(child);
    let _ = fs::remove_dir_all(&root);
}

/// An unreadable state root is a visible open failure, not "no runs" (review-3b).
#[test]
fn an_unreadable_state_root_is_an_open_failure() {
    let root = scratch("state-root-perm");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    layout.ensure_root().expect("root");
    let state = layout.state_root().to_path_buf();
    fs::create_dir_all(&state).expect("state root");
    settings_with_agent(&root);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o000))
        .expect("make the state root unreadable");
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let error = match engine::open_with(options) {
        Ok(_) => panic!("the engine opened with an unreadable state root"),
        Err(error) => error,
    };
    assert!(
        error.contains("state root") && error.contains("cannot be read"),
        "the open failure does not name the state root: {error}"
    );
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("repair");
    let _ = fs::remove_dir_all(&root);
}

/// A denied process-record metadata read is a failure, not an absent record.
#[test]
fn a_denied_record_metadata_is_a_visible_failure() {
    let root = scratch("record-metadata");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000023-0001");
    mark_run_started(&run_dir);
    fs::write(run_dir.join("shell.pid"), "12345").expect("shell pid");
    fs::set_permissions(run_dir.join("shell.pid"), fs::Permissions::from_mode(0o000))
        .expect("deny the record");
    settings_with_agent(&root);
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let reason = engine.board().agents[0]
        .leftover
        .clone()
        .expect("the denied record is not on the card");
    assert!(
        reason.contains("cannot be read") || reason.contains("cannot be inspected"),
        "the card reason is not the denied record: {reason}"
    );
    fs::set_permissions(run_dir.join("shell.pid"), fs::Permissions::from_mode(0o600))
        .expect("repair the record");
    engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start retries after the denied record is repaired");
    assert!(
        engine.board().agents[0].leftover.is_none(),
        "the repaired record did not clear the leftover"
    );
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// An unrelated file reader that merely has the run directory in its argv (the
/// reviewer's `find <run_dir> -exec sleep 60 ;`) never inherits the run's
/// capability and must not be signalled from stale numeric records (review-c4).
#[test]
fn a_find_reader_of_the_run_directory_is_not_owned() {
    let _process_test = process_test_lock();
    let root = scratch("find-reader");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000025-0001");
    let mut reader = Command::new("/usr/bin/find")
        .arg(&run_dir)
        .args(["-exec", "sleep", "30", ";"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the unrelated find reader starts");
    let pid = reader.id();
    assert!(
        wait_until(Duration::from_secs(5), || unrelated_alive(pid)),
        "the unrelated find reader did not start"
    );
    // Stale numeric records name the reader's group.
    fs::write(run_dir.join("session.pid"), pid.to_string()).expect("session pid");
    fs::write(run_dir.join("session.pgid"), pid.to_string()).expect("session pgid");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("a run-directory argv is not ownership");
    assert!(
        error.contains("cannot be verified") || error.contains("cannot be read"),
        "the unowned reader was not reported: {error}"
    );
    assert!(
        unrelated_alive(pid),
        "an unrelated find reader was killed from a run-directory argv"
    );
    assert!(
        !run_dir.join("run-stopped.json").is_file(),
        "a false run_stopped was written for the file reader"
    );
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = reader.wait();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn fx_r34_inaccessible_capability_helper() {
    if std::env::var_os("GEWU_FX_R34_INACCESSIBLE_CAPABILITY").is_none() {
        return;
    }
    let _ = rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable);
    std::thread::sleep(Duration::from_secs(30));
}

fn spawn_inaccessible_reader() -> Child {
    let child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "fx_r34_inaccessible_capability_helper",
            "--nocapture",
        ])
        .env("GEWU_FX_R34_INACCESSIBLE_CAPABILITY", "1")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("unrelated non-dumpable process");
    assert!(wait_until(Duration::from_secs(5), || {
        fs::read_dir(format!("/proc/{}/fd", child.id())).is_err()
    }));
    child
}

#[test]
fn an_inaccessible_unrelated_process_does_not_hide_a_readable_holder() {
    let _process_test = process_test_lock();
    // Start this process before the capability exists: it cannot inherit it.
    let mut reader = spawn_inaccessible_reader();
    let root = scratch("readable-holder");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000031-0001");
    let capability = run::prepare_shell_capability(&run_dir).expect("capability");
    let mut holder = spawn_unrelated_group("readable-holder");
    drop(capability);
    let result = run::reap_leftover_run(&run_dir, Duration::from_millis(150));
    let stopped = wait_until(Duration::from_secs(2), || {
        holder.try_wait().is_ok_and(|status| status.is_some())
    });
    if !stopped {
        stop_unrelated(holder);
    }
    let reader_survived = reader.try_wait().expect("unrelated process status").is_none();
    if reader_survived {
        stop_unrelated(reader);
    }
    assert!(result.expect("unrelated inspection failure must not hide the holder"));
    assert!(stopped, "the verified holder survived");
    assert!(reader_survived, "the unrelated reader was signalled");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_inaccessible_unrelated_process_does_not_block_previous_boot_disposition() {
    let _process_test = process_test_lock();
    let reader = spawn_inaccessible_reader();
    let root = scratch("previous-boot-capability");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000032-0001");
    let capability = run::prepare_shell_capability(&run_dir).expect("capability");
    drop(capability);
    let old = fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
    for name in ["plan.json", "shell.capability"] {
        fs::File::open(run_dir.join(name))
            .expect("record")
            .set_times(old)
            .expect("old record");
    }
    let result = run::reap_leftover_run(&run_dir, Duration::from_millis(150));
    stop_unrelated(reader);
    assert!(!result.expect("a reboot proves no old holder can survive"));
    assert!(run_dir.join("run-stopped.json").is_file());
    assert!(!run_dir.join("shell.capability").exists());
    let _ = fs::remove_dir_all(root);
}

/// A durable capability record plus a genuine non-dumpable holder whose fd table
/// is unreadable is uncertainty, not "never started": it must reach the card and
/// block Start while keeping the capability evidence (review-c4).
#[test]
fn an_inaccessible_capability_holder_blocks_start_and_keeps_evidence() {
    let _process_test = process_test_lock();
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let root = scratch("inaccessible-capability");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_dir = write_run(&layout, "agent-1", "run-1700000000026-0001");
    let fd = nix::sys::memfd::memfd_create(
        "gewu-inaccessible-capability",
        nix::sys::memfd::MFdFlags::empty(),
    )
    .expect("memfd");
    let mut capability = fs::File::from(fd);
    let token = [0xa5_u8; 16];
    capability.write_all(&token).expect("token");
    capability.seek(SeekFrom::Start(0)).expect("seek");
    let metadata = capability.metadata().expect("metadata");
    fs::write(
        run_dir.join("shell.capability"),
        format!(
            "{} {} {}\n",
            metadata.dev(),
            metadata.ino(),
            token
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    )
    .expect("capability record");
    let mut holder = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "fx_r34_inaccessible_capability_helper",
            "--nocapture",
        ])
        .env("GEWU_FX_R34_INACCESSIBLE_CAPABILITY", "1")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the inaccessible holder starts");
    let holder_pid = holder.id();
    drop(capability);
    assert!(
        wait_until(Duration::from_secs(5), || {
            fs::read_dir(format!("/proc/{holder_pid}/fd")).is_err()
        }),
        "the holder's fd table did not become inaccessible"
    );
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(150))
        .expect_err("an inaccessible capability holder is unresolved uncertainty");
    assert!(
        error.contains("file descriptors cannot be read") || error.contains("cleanup is uncertain"),
        "the inaccessible holder was not reported: {error}"
    );
    assert!(
        run_dir.join("shell.capability").is_file(),
        "the capability evidence was cleared while the holder was inaccessible"
    );
    assert!(
        !run_dir.join("run-stopped.json").is_file(),
        "a false run_stopped was written for the inaccessible holder"
    );
    // Actual engine view/card/Start propagation.
    settings_with_agent(&root);
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let reason = engine.board().agents[0]
        .leftover
        .clone()
        .expect("the inaccessible holder is not on the card");
    assert!(
        reason.contains("file descriptors cannot be read")
            || reason.contains("cleanup is uncertain"),
        "the card reason is not the inaccessible holder: {reason}"
    );
    engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect_err("Start must not replace a run whose holder cannot be inspected");
    assert!(
        run_dir.join("shell.capability").is_file(),
        "Start cleared the capability evidence"
    );
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(holder_pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = holder.wait();
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}

/// The reviewer's `orphan-no-pid` probe: a live shell whose supervisor is gone
/// and whose `session.pid`/`session.pgid` records were removed. The run
/// directory's own shell record must still lead a reopen to the group (F-02/F-03
/// review blocker 6).
#[test]
fn a_shell_whose_session_pid_is_gone_is_reaped_from_the_run_directory() {
    let _process_test = process_test_lock();
    let root = scratch("orphan-no-pid");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let run_id = "run-1700000000006-0001";
    let run_dir = write_run(&layout, "agent-1", run_id);
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    write_records(&run_dir, pgid);
    assert!(
        wait_until(Duration::from_secs(10), || {
            run_dir.join("shell.pgid").is_file() && !shell_members(pgid).is_empty()
        }),
        "the shell or its run-directory record never appeared"
    );
    // Kill only the supervisor; the shell stays in the group. Then remove the
    // supervisor's own records exactly as a clean exit would have done.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("kill the supervisor");
    let _ = supervisor.wait();
    let _ = fs::remove_file(run_dir.join("session.pid"));
    let _ = fs::remove_file(run_dir.join("session.pgid"));
    assert!(
        run_dir.join("shell.pgid").is_file(),
        "the shell's own record is gone; the test cannot prove the fallback"
    );
    assert!(
        !shell_members(pgid).is_empty(),
        "the shell did not survive; the test cannot prove the reap"
    );
    // Reopen the engine: recovery runs before it opens.
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("the engine opens");
    let _ = engine.board();
    assert!(
        shell_members(pgid).is_empty(),
        "the reopen left the orphaned shell running: {:?}",
        shell_members(pgid)
    );
    assert!(
        run_dir.join("run-stopped.json").is_file(),
        "no run_stopped record for the orphan"
    );
    assert!(
        !run_dir.join("shell.pgid").is_file() && !run_dir.join("session.pid").is_file(),
        "the reaped run kept a stale process record"
    );
    drop(engine);
    let _ = fs::remove_dir_all(&root);
}
/// The product stand-in for the live kill test: start one run, then stay alive
/// until the test kills this whole process. The run agent asks the kernel to end
/// it when this process dies, which leaves its shell behind for the reopen to reap.
#[test]
fn fx_r34_product_parent_helper() {
    let Ok(run_dir) = std::env::var("GEWU_FX_R34_HELPER_RUN") else {
        return;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("the helper's run starts");
    std::thread::sleep(Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();
}

// ---- remaining fx-r34 guards ---------------------------------------------

#[test]
fn a_wrong_address_cannot_be_continued_past_and_an_untested_one_says_so() {
    use gewu_multiverse::board::{FakeEngine, Screen};
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), Screen::Model);
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for ch in "http://127.0.0.1:4780".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    board.on_key(KeyEvent::from(KeyCode::Enter));
    assert!(!board.model_editing(), "⏎ left the next field in edit mode");
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for ch in "demo".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    board.on_key(KeyEvent::from(KeyCode::Enter));
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for ch in "key".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    board.on_key(KeyEvent::from(KeyCode::Enter));
    board.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(
        board.screen(),
        Screen::Model,
        "a wrong address was continued"
    );
    assert!(
        board.notice().unwrap_or_default().contains("wrong address"),
        "{:?}",
        board.notice()
    );
    // A valid but untested address continues with the honest line.
    board.on_key(KeyEvent::from(KeyCode::Up));
    board.on_key(KeyEvent::from(KeyCode::Up));
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for _ in 0..40 {
        board.on_key(KeyEvent::from(KeyCode::Backspace));
    }
    for ch in "https://model.example/v1/chat/completions".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    board.on_key(KeyEvent::from(KeyCode::Enter));
    board.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(board.screen(), Screen::Picker);
    assert!(
        board.notice().unwrap_or_default().contains("not tested"),
        "{:?}",
        board.notice()
    );
}

#[test]
fn ctrl_u_clears_a_field_and_the_mask_has_one_length() {
    use gewu_multiverse::board::FakeEngine;
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    let mut board = Board::new(engine);
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for ch in "abcdef".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    board.on_key(KeyEvent::new(
        KeyCode::Char('u'),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    ));
    let frame = frame_text(&draw(&mut board, 80, 24));
    assert!(
        !frame.contains("abcdef"),
        "Ctrl-U did not clear the field:\n{frame}"
    );
}

#[test]
fn a_read_only_check_is_no_work_and_a_clean_write_is_a_success() {
    use gewu_multiverse::agent_run::{RunEnding, ending_for};
    assert_eq!(ending_for("nothing new to add"), RunEnding::NoWork);
    assert_eq!(ending_for("I fixed the flaky test."), RunEnding::Succeeded);
}

#[test]
fn a_no_work_backoff_says_why_and_a_failed_run_says_http_on_the_board() {
    use gewu_multiverse::board::status;
    let mut board_view = view(1);
    board_view.agents[0].session = SessionState::Restarting {
        next_at: SystemTime::now() + Duration::from_secs(20),
        last: SessionEnd::NoWork {
            reason: "nothing new".to_owned(),
        },
    };
    let face = status::agent_face(&board_view.agents[0]);
    assert!(face.status.contains("no work"), "{}", face.status);
    board_view.agents[0].session = SessionState::Idle;
    board_view.agents[0].recent = vec![SessionEnd::Failed {
        reason: "the model call failed (server_error): the model answered HTTP 500".to_owned(),
    }];
    let mut board = Board::new(Fixed(board_view));
    let frame = frame_text(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("HTTP 500"),
        "the board hides the outage:\n{frame}"
    );
}

#[test]
fn upgrade_names_this_releases_change_and_reshows_changed_warnings() {
    let root = scratch("upgrade");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    layout.ensure_root().expect("root");
    gewu_multiverse::engine::last_seen::write(&root, "0.1.32").expect("old version");
    // A real v0.1.32-shaped home: the endpoint is configured, three agents exist,
    // and the recorded warning wording is older than this build's.
    let mut settings = gewu_multiverse::engine::settings::Settings {
        terms_acknowledged: true,
        terms_version: 0,
        ..Default::default()
    };
    settings.endpoint.url = "https://model.example/v1/chat/completions".to_owned();
    settings.endpoint.model = "demo-model".to_owned();
    for number in 1..=3 {
        settings
            .add_agent(&format!("agent-{number}"), Desired::Stopped)
            .expect("an agent");
    }
    settings
        .save(&gewu_multiverse::engine::settings::path_in(&root))
        .expect("settings");
    let mut options = EngineOptions::new(layout.clone(), root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    assert_eq!(
        gewu_multiverse::engine::last_seen::read(layout.root()).as_deref(),
        Some("0.1.32"),
        "root {:?}",
        layout.root()
    );
    let mut engine = engine::open_with(options).expect("engine");
    assert!(engine.board().upgrade.is_some(), "no upgrade view");
    let mut board = Board::new(engine);
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Upgrade);
    let upgrade: UpgradeView = board.view().upgrade.clone().expect("upgrade");
    assert!(!upgrade.whats_new.is_empty());
    assert!(upgrade.warnings_changed);
    let frame = frame_text(&draw(&mut board, 80, 24));
    assert!(frame.contains("new:"), "{frame}");
    assert!(frame.contains("warnings changed"), "{frame}");
    board.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Terms);
    // Read the warnings, then agree. At the floor the warnings can need a scroll
    // first; once every item is on the frame `y` records them.
    let mut home = false;
    for _ in 0..12 {
        let _ = draw(&mut board, 80, 24);
        board.on_key(KeyEvent::from(KeyCode::Char('y')));
        if board.screen() == gewu_multiverse::board::Screen::Home {
            home = true;
            break;
        }
        board.on_key(KeyEvent::from(KeyCode::Down));
    }
    assert!(
        home,
        "an upgrade with changed warnings never returned to the board: {:?}",
        board.screen()
    );
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Home);
    assert_eq!(
        board.view().agents.len(),
        3,
        "the existing agents are still the person's agents"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_newest_repository_event_survives_a_reopen() {
    let root = scratch("event");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    layout.ensure_root().expect("root");
    let mut settings = gewu_multiverse::engine::settings::Settings {
        terms_acknowledged: true,
        terms_version: TERMS_VERSION,
        ..Default::default()
    };
    settings.forgejo.last_event = Some("team/scratch was deleted".to_owned());
    settings
        .save(&gewu_multiverse::engine::settings::path_in(&root))
        .expect("settings");
    let mut options = EngineOptions::new(layout, root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("engine");
    let view = engine.board();
    assert!(
        view.forgejo
            .events
            .iter()
            .any(|event| event.contains("team/scratch was deleted"))
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_readable_transcript_is_human_and_the_jsonl_stays() {
    let root = scratch("transcript");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    layout.ensure_root().expect("root");
    let paths = layout.ensure_agent("agent-1").expect("paths");
    let run_id = "run-1700000000003-0001";
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("run dir");
    let mut transcript =
        gewu_multiverse::transcript::Transcript::open(&run_dir).expect("transcript");
    transcript
        .append(&gewu_multiverse::transcript::Record::run_started(
            run_id,
            "agent-1",
            7,
            "/tmp/work",
        ))
        .expect("started");
    transcript
        .append(&gewu_multiverse::transcript::Record::run_finished(
            run_id,
            gewu_multiverse::run::RunOutcome::Succeeded,
            Some(0),
            10,
            Some("all done".to_owned()),
        ))
        .expect("finished");
    let text = run::readable_transcript(&run_dir);
    assert!(
        text.contains(&format!("run {run_id} of agent-1 started")),
        "{text}"
    );
    assert!(
        text.contains("run finished: succeeded — all done"),
        "{text}"
    );
    assert!(!text.contains("\"event\""), "{text}");
    let rendered = run::write_readable_transcript(&run_dir).expect("readable file");
    assert!(rendered.is_file());
    assert!(run_dir.join("transcript.jsonl").is_file());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn copied_is_shown_and_the_quit_line_names_no_blocked_agent() {
    use gewu_multiverse::board::FakeEngine;
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    engine
        .send(BoardCommand::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(BoardCommand::CreateAgents { count: 2 })
        .expect("agents");
    engine.send(BoardCommand::StartForgejo).expect("forgejo");
    for _ in 0..140 {
        engine.tick();
    }
    let _ = engine.board();
    // The second agent has no instruction and stays blocked; only the first is running.
    let path = engine.instruction_path("agent-1").expect("path");
    fs::write(path, "work\n").expect("instruction");
    engine
        .send(BoardCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    engine.tick();
    let mut board = Board::new(engine);
    board.on_key(KeyEvent::from(KeyCode::Char('q')));
    let frame = frame_text(&draw(&mut board, 80, 24));
    assert!(frame.contains("quit stops agent-1 and Forgejo"), "{frame}");
    assert!(!frame.contains("agent-1 and agent-2"), "{frame}");
}

#[test]
fn c_after_p_shows_the_copied_word() {
    let mut board = board_for_forgejo();
    board.on_key(KeyEvent::from(KeyCode::Char('f')));
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Forgejo);
    board.on_key(KeyEvent::from(KeyCode::Char('p')));
    assert!(
        board.revealed_password().is_some(),
        "no password was revealed"
    );
    board.on_key(KeyEvent::from(KeyCode::Char('c')));
    assert_eq!(board.notice(), Some("copied"));
}

fn board_for_model() -> Board {
    use gewu_multiverse::board::FakeEngine;
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    engine
        .send(BoardCommand::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("a-very-long-saved-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(BoardCommand::CreateAgents { count: 1 })
        .expect("agent");
    engine.send(BoardCommand::StartForgejo).expect("forgejo");
    for _ in 0..140 {
        engine.tick();
    }
    let _ = engine.board();
    let mut board = Board::new(engine);
    board.on_key(KeyEvent::from(KeyCode::Char('m')));
    board
}

fn board_for_forgejo() -> Board {
    use gewu_multiverse::board::FakeEngine;
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    engine
        .send(BoardCommand::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(BoardCommand::CreateAgents { count: 1 })
        .expect("agent");
    engine.send(BoardCommand::StartForgejo).expect("forgejo");
    for _ in 0..140 {
        engine.tick();
    }
    let _ = engine.board();
    Board::new(engine)
}

#[test]
fn the_warnings_screen_is_one_compact_numbered_block_with_its_count() {
    use gewu_multiverse::board::FakeEngine;
    for (width, height) in SIZES {
        let mut board = Board::new(FakeEngine::new());
        assert_eq!(board.screen(), gewu_multiverse::board::Screen::Terms);
        let frame = frame_text(&draw(&mut board, width, height));
        let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("5 warnings"),
            "{width}x{height}: the count is not stated:\n{flat}"
        );
        assert!(
            flat.contains("runs on this computer are confined (Landlock)"),
            "{width}x{height}: the factual confinement line is missing:\n{flat}"
        );
        // The block is compact: no more than one blank row between the numbered
        // items (leading and trailing framing rows are not part of the block).
        let lines: Vec<&str> = frame.lines().collect();
        let start = lines
            .iter()
            .position(|line| line.contains("Agents use this computer"))
            .expect("item 1");
        let end = lines
            .iter()
            .position(|line| line.contains("Their work is sent"))
            .expect("item 5");
        let mut run = 0;
        for line in &lines[start..=end] {
            let content = line
                .chars()
                .filter(|ch| !matches!(ch, '│' | '╭' | '╮' | '╰' | '╯' | '─'))
                .collect::<String>();
            if content.trim().is_empty() {
                run += 1;
                assert!(
                    run <= 1,
                    "{width}x{height}: a gap inside the warnings:\n{frame}"
                );
            } else {
                run = 0;
            }
        }
    }
}

#[test]
fn the_legend_uses_no_work_at_every_size() {
    use gewu_multiverse::board::FakeEngine;
    let mut engine = FakeEngine::new();
    engine.send(BoardCommand::AcknowledgeTerms).expect("terms");
    engine
        .send(BoardCommand::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(BoardCommand::CreateAgents { count: 1 })
        .expect("agent");
    engine.send(BoardCommand::StartForgejo).expect("forgejo");
    for _ in 0..140 {
        engine.tick();
    }
    let _ = engine.board();
    let mut board = Board::new(engine);
    board.on_key(KeyEvent::from(KeyCode::Enter));
    for (width, height) in SIZES {
        let frame = frame_text(&draw(&mut board, width, height));
        let flat = frame.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("∅ no work"), "{width}x{height}:\n{flat}");
        assert!(flat.contains("◷ timed out"), "{width}x{height}:\n{flat}");
        assert!(!flat.contains("∅ none"), "{width}x{height}:\n{flat}");
    }
}

#[test]
fn the_model_form_masks_a_saved_and_an_edited_key_with_one_length() {
    let mut board = board_for_model();
    let saved = frame_text(&draw(&mut board, 80, 24));
    let saved_mask = saved
        .lines()
        .find(|line| line.contains('•'))
        .expect("a saved mask")
        .chars()
        .filter(|ch| *ch == '•')
        .count();
    assert_eq!(saved_mask, 8, "{saved}");
    board.on_key(KeyEvent::from(KeyCode::Down));
    board.on_key(KeyEvent::from(KeyCode::Down));
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    for ch in "new-key".chars() {
        board.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    let edited = frame_text(&draw(&mut board, 80, 24));
    let edited_mask = edited
        .lines()
        .find(|line| line.contains('•'))
        .expect("an edited mask")
        .chars()
        .filter(|ch| *ch == '•')
        .count();
    assert_eq!(edited_mask, 8, "{edited}");
    assert!(
        !saved.contains(" —"),
        "a stray dash is on the key row:\n{saved}"
    );
}

#[test]
fn a_test_result_clears_on_the_first_edit() {
    let mut board = board_for_model();
    board.on_key(KeyEvent::from(KeyCode::Char('t')));
    board.tick();
    let tested = frame_text(&draw(&mut board, 80, 24));
    assert!(tested.contains("✓ pong"), "{tested}");
    board.on_key(KeyEvent::from(KeyCode::Char('e')));
    board.on_key(KeyEvent::from(KeyCode::Char('x')));
    let edited = frame_text(&draw(&mut board, 80, 24));
    assert!(
        !edited.contains("✓ pong"),
        "a stale test result is still beside an edited value:\n{edited}"
    );
}

#[test]
fn the_agent_panel_is_content_sized_and_the_marks_sit_under_the_instruction() {
    let mut board = Board::new(Fixed(view(1)));
    board.on_key(KeyEvent::from(KeyCode::Enter));
    let frame = frame_text(&draw(&mut board, 200, 60));
    let lines: Vec<&str> = frame.lines().collect();
    let instruction = lines
        .iter()
        .position(|line| line.contains("instruction"))
        .expect("instruction heading");
    let marks = lines
        .iter()
        .position(|line| line.contains("last runs"))
        .expect("run marks");
    assert!(
        marks.saturating_sub(instruction) <= 8,
        "the marks are detached from the instruction ({} rows):\n{frame}",
        marks - instruction
    );
}
