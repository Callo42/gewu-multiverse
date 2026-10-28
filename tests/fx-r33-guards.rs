//! Guards for the wave-32 polish findings (fx-r33).
//!
//! This file deliberately uses only APIs that existed at `d99e6a1`, so it can be
//! copied into a base worktree and each assertion names the behaviour the base
//! lacked. Guards that need the new view fields or helpers live in
//! `fx-r33-new-api.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnding};
use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::bundled_forgejo::{self, InstancePaths, InstanceSecrets};
use gewu_multiverse::engine::settings::Settings;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{BoardView, Command, Desired, EndpointTest, EngineApi};
use gewu_multiverse::model::{self, ModelError};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Color;

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
}

fn type_text(board: &mut Board, text: &str) {
    for ch in text.chars() {
        press(board, KeyCode::Char(ch));
    }
}

fn draw(buffer_board: &mut Board, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| buffer_board.draw(frame))
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

fn ready(count: usize) -> FakeEngine {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo-model".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    // The picker makes at most twelve at once; the settings allow twenty over
    // time, so the crowded guards add the rest one at a time.
    let first = count.min(12);
    engine
        .send(Command::CreateAgents { count: first })
        .expect("agents");
    for _ in first..count {
        engine.send(Command::AddAgent).expect("another agent");
    }
    engine.send(Command::StartForgejo).expect("Forgejo");
    for _ in 0..(120 + count * 2) {
        engine.tick();
    }
    let _ = engine.board();
    engine
}

struct FactsEngine(FakeEngine);

impl EngineApi for FactsEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.repository_names = vec![
            "team/scratch-mei".to_owned(),
            "team/website".to_owned(),
            "team/workspace".to_owned(),
        ];
        view.forgejo.repositories = 3;
        view.forgejo.events = vec!["team/scratch-mei was deleted".to_owned()];
        view.forgejo.admin_login = "mei".to_owned();
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

struct MarkEngine(FakeEngine);

impl EngineApi for MarkEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![
                gewu_multiverse::engine_api::SessionEnd::Succeeded,
                gewu_multiverse::engine_api::SessionEnd::Failed {
                    reason: "model".to_owned(),
                },
                gewu_multiverse::engine_api::SessionEnd::TimedOut,
                gewu_multiverse::engine_api::SessionEnd::Refused {
                    reason: "server refused the write".to_owned(),
                },
            ];
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

struct ProviderEngine(FakeEngine);

impl EngineApi for ProviderEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.endpoint.test = EndpointTest::Failed {
            reason: "add /v1 — the provider answered HTTP 404: Unknown route /chat/completions. \
                     This provider serves the OpenAI-compatible API under /v1/chat/completions. \
                     Add /v1 to the base URL."
                .to_owned(),
        };
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

/// The long repository event the footer guards use: it cannot share a 72-column
/// line with the keys without cutting one of them.
const LONG_EVENT: &str = "team/another-very-long-repository-name-for-overlap-33 was created";

struct LongEventEngine(FakeEngine);

impl EngineApi for LongEventEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.events = vec![LONG_EVENT.to_owned()];
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

/// The repository names the review used: the first two fill the 80-column line, so
/// the base drops the third with no `(+N)` at all.
struct ReviewRepoEngine(FakeEngine);

impl EngineApi for ReviewRepoEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.repository_names = vec![
            "team/scratch-rv".to_owned(),
            "team/scratch-rv2".to_owned(),
            "team/scratch-rv3".to_owned(),
        ];
        view.forgejo.repositories = 3;
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

/// A home whose selected agent carries the reason a real model-down run recorded.
struct DownEngine(FakeEngine, String);

impl EngineApi for DownEngine {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.session = gewu_multiverse::engine_api::SessionState::Idle;
            agent.desired = Desired::Stopped;
            agent.last_error = Some(self.1.clone());
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
fn the_crowded_floor_puts_every_agent_on_the_frame() {
    for count in [4, 7, 12] {
        let mut board = Board::new(ready(count));
        let frame = text(&draw(&mut board, 72, 18));
        let words: Vec<&str> = frame.split_whitespace().collect();
        for number in 1..=count {
            let name = format!("agent-{number}");
            let login = format!("gewu-agent-{number}");
            assert!(
                words.contains(&name.as_str()),
                "B1: {name} is not on the 72x18 frame with {count} agents:\n{frame}"
            );
            assert!(
                words.contains(&login.as_str()),
                "B1: {login} is not on the 72x18 frame with {count} agents:\n{frame}"
            );
        }
    }
}

#[test]
fn the_crowded_eighty_column_board_keeps_its_facts() {
    for count in [7, 12] {
        let mut board = Board::new(FactsEngine(ready(count)));
        let frame = text(&draw(&mut board, 80, 24));
        for wanted in [
            "team · 3 repositories",
            "your login: mei",
            "17.0.0",
            "team/scratch-mei was deleted",
        ] {
            assert!(
                frame.contains(wanted),
                "B2: {wanted} is missing from the 80x24 board with {count} agents:\n{frame}"
            );
        }
    }
}

#[test]
fn the_72_footer_puts_the_event_on_its_own_row() {
    let keys = "f Forgejo   m model   ? help   q quit";
    // One agent: the old footer wrote the long event from column 1 over the
    // second key line. Twelve: a crowded card grid. Twenty: the scroll indicator
    // shares that key line and must not take the event with it.
    for (count, scrolls) in [(1, false), (12, false), (20, true)] {
        let mut board = Board::new(LongEventEngine(ready(count)));
        let frame = text(&draw(&mut board, 72, 18));
        assert!(
            frame.contains(keys),
            "B3: the key hints are not whole at 72x18 with {count} agents:\n{frame}"
        );
        assert!(
            frame.contains("another-very-long-repository-name"),
            "B3: the newest event is missing at 72x18 with {count} agents:\n{frame}"
        );
        for line in frame.lines() {
            if line.contains("f Forgejo") {
                assert!(
                    !line.contains("another-very"),
                    "B3: the event shares the keys' row at 72x18 with {count} agents:\n{frame}"
                );
            }
        }
        assert_eq!(
            board.layout().scrolls,
            scrolls,
            "B3: the scroll state changed with {count} agents"
        );
        if scrolls {
            let footer = frame
                .lines()
                .find(|line| line.contains("f Forgejo"))
                .expect("the key line");
            assert!(
                footer.contains("/ 20"),
                "B3: the scroll indicator is not beside the keys:\n{frame}"
            );
        }
    }
}

#[test]
fn the_repository_line_reserves_room_for_the_hidden_count() {
    let mut board = Board::new(ReviewRepoEngine(ready(3)));
    let frame = text(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("team · 3 repositories"),
        "NB1: the repository count is missing:\n{frame}"
    );
    // The first two names fill the line at 80; the third is hidden and the tag
    // only fits by dropping the second name.
    assert!(
        frame.contains("(+1)") || frame.contains("(+2)"),
        "NB1: hidden repository names carry no count:\n{frame}"
    );
    assert!(
        !frame.contains("tea…") && !frame.contains("team/scratch-rv…"),
        "NB1: a repository name was cut:\n{frame}"
    );
}

#[test]
fn the_model_down_row_keeps_the_errno_on_the_72_card() {
    let root = scratch_root("model-down-72");
    let run_dir = prepare(&root, "http://127.0.0.1:1/v1/chat/completions");
    let _ = agent_run::execute(&run_dir);
    let reason = transcript::terminal_record(&run_dir)
        .and_then(|record| match record {
            transcript::Record::RunFinished { reason, .. } => reason,
            _ => None,
        })
        .expect("the failed run's reason");
    let mut board = Board::new(DownEngine(ready(1), reason));
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 72, 18));
    assert!(
        frame.contains("(errno 111)"),
        "NB2: the 72x18 card does not keep the errno:\n{frame}"
    );
    assert!(
        !frame.contains("os error"),
        "NB2: the transport's own wording reached the 72x18 card:\n{frame}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_floor_board_shows_every_agent() {
    let mut board = Board::new(ready(3));
    let frame = text(&draw(&mut board, 72, 18));
    for wanted in ["agent-1", "agent-2", "agent-3", "gewu-agent-3"] {
        assert!(
            frame.contains(wanted),
            "F-03/U1: {wanted} is off the 72x18 board:\n{frame}"
        );
    }
}

#[test]
fn the_eighty_column_board_keeps_the_forgejo_facts() {
    let mut board = Board::new(FactsEngine(ready(3)));
    let frame = text(&draw(&mut board, 80, 24));
    for wanted in [
        "team · 3 repositories",
        "team/website",
        "your login: mei",
        "17.0.0",
        "team/scratch-mei was deleted",
    ] {
        assert!(
            frame.contains(wanted),
            "U1/F-05/F-09: {wanted} missing from the 80x24 board:\n{frame}"
        );
    }
}

#[test]
fn the_72_board_and_forgejo_page_show_the_newest_event() {
    let mut board = Board::new(FactsEngine(ready(3)));
    let frame = text(&draw(&mut board, 72, 18));
    assert!(
        frame.contains("team/scratch-mei was deleted"),
        "F-05: the board drops the event at 72x18:\n{frame}"
    );
    press(&mut board, KeyCode::Char('f'));
    let page = text(&draw(&mut board, 72, 18));
    assert!(
        page.contains("team/scratch-mei was deleted"),
        "F-05: the Forgejo page drops the event at 72x18:\n{page}"
    );
}

#[test]
fn the_72_legend_is_whole_and_the_newest_mark_has_a_word() {
    let mut board = Board::new(MarkEngine(ready(1)));
    press(&mut board, KeyCode::Enter);
    let buffer = draw(&mut board, 72, 18);
    let frame = text(&buffer);
    let strip = frame
        .lines()
        .find(|line| line.contains("last runs"))
        .unwrap_or_else(|| panic!("F-04: no run strip:\n{frame}"));
    assert!(
        strip.contains("refused"),
        "F-04: the newest mark has no word beside it: {strip:?}"
    );
    let flat = flat(&buffer);
    assert!(
        flat.contains("■ stopped") && flat.contains("◷ timed out"),
        "F-04: the 72 legend is not whole: {flat}"
    );
    assert!(
        flat.contains("∅ no work"),
        "F-04: the no-work mark has no whole 72-column word: {flat}"
    );
}

#[test]
fn a_refused_read_is_a_warning_and_a_refused_write_is_the_write_mark() {
    assert_eq!(
        agent_run::ending_for("I could not read the protected file: Permission denied."),
        RunEnding::Warned,
        "F-02: a refused read was marked as a refused write"
    );
    assert_eq!(
        agent_run::ending_for("I could not delete team/library."),
        RunEnding::Refused,
        "F-02: a refused destructive instruction lost its write mark"
    );
}

#[test]
fn the_model_error_shows_the_providers_whole_answer_at_eighty() {
    let mut board = Board::new(ProviderEngine(ready(1)));
    press(&mut board, KeyCode::Char('m'));
    let flat = flat(&draw(&mut board, 80, 24));
    assert!(
        flat.contains("This provider serves the OpenAI-compatible API under /v1/chat/completions"),
        "F-07: the provider's second sentence is dropped at 80x24: {flat}"
    );
    assert!(
        flat.contains("Add /v1 to the base URL"),
        "F-07: the provider's repair sentence is dropped at 80x24: {flat}"
    );
}

#[test]
fn the_provider_failure_words_keep_the_whole_message() {
    let words = model::failure_words(
        &ModelError::RouteNotFound {
            endpoint: "http://127.0.0.1:1/chat/completions".to_owned(),
            status: 404,
            message: "Unknown route /chat/completions. This provider serves the API under /v1. \
                      Add /v1 to the base URL."
                .to_owned(),
        },
        "demo",
    );
    assert!(words.contains("This provider serves"), "{words}");
    assert!(words.contains("Add /v1 to the base URL"), "{words}");
}

#[test]
fn the_quit_warning_names_the_agents() {
    let mut engine = ready(3);
    for name in ["agent-1", "agent-2"] {
        let path = engine.instruction_path(name).expect("an instruction path");
        std::fs::write(path, "work the issue\n").expect("the instruction");
    }
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start agent-1");
    engine
        .send(Command::Start {
            name: "agent-2".to_owned(),
        })
        .expect("start agent-2");
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Char('q'));
    let flat = flat(&draw(&mut board, 80, 24));
    assert!(
        flat.contains("quit stops agent-1 and agent-2, and Forgejo"),
        "F-08: the quit warning names a count, not the agents: {flat}"
    );
}

#[test]
fn the_warnings_screen_names_this_computers_confinement() {
    let mut board = Board::new(FakeEngine::new());
    for (width, height) in [(72, 18), (80, 24)] {
        let page = text(&draw(&mut board, width, height));
        assert!(
            page.contains("This computer confines runs (Landlock)"),
            "F-06: no positive confinement line at {width}x{height}:\n{page}"
        );
    }
}

#[test]
fn removing_the_last_agent_says_why() {
    let mut board = Board::new(ready(1));
    press(&mut board, KeyCode::Char('-'));
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("the last agent cannot be removed")),
        "U11: the last-agent removal was silent or unexplained: {:?}",
        board.notice()
    );
}

#[test]
fn adding_after_a_removal_never_reuses_the_agent_number() {
    let mut settings = Settings::default();
    settings
        .add_agent("agent-1", Desired::Stopped)
        .expect("one");
    settings
        .add_agent("agent-2", Desired::Stopped)
        .expect("two");
    assert!(settings.remove_agent("agent-2"));
    assert_eq!(
        settings.next_free_name(),
        "agent-3",
        "U5: the removed agent's number and account would be resurrected"
    );
}

#[test]
fn the_model_form_names_field_movement_and_enter_moves_on() {
    let mut board = Board::new(FakeEngine::new());
    let _ = draw(&mut board, 80, 24);
    press(&mut board, KeyCode::Char('y'));
    press(&mut board, KeyCode::Char('e'));
    type_text(&mut board, "x");
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("▸ model"),
        "U2: ⏎ did not move from the address to the model field:\n{frame}"
    );
    assert!(
        frame.contains("↑↓ field"),
        "U2: the form does not say how to move between fields:\n{frame}"
    );
}

#[test]
fn the_72_instruction_keeps_its_first_line_and_a_tall_panel_shows_more() {
    let mut engine = ready(1);
    let path = engine
        .instruction_path("agent-1")
        .expect("an instruction path");
    fs::write(
        &path,
        "first line of the instruction\nsecond line of the instruction\n",
    )
    .expect("the instruction");
    engine.tick();
    let _ = engine.board();
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    let floor = text(&draw(&mut board, 72, 18));
    assert!(
        floor.contains("first line of the instruction"),
        "U4/F-11: the first instruction line disappeared at 72x18:\n{floor}"
    );
    let tall = text(&draw(&mut board, 200, 60));
    assert!(
        tall.contains("second line of the instruction"),
        "U4/F-11: a tall panel did not use its spare rows for the instruction:\n{tall}"
    );
}

#[test]
fn the_repository_line_never_cuts_a_name() {
    let mut board = Board::new(FactsEngine(ready(3)));
    let frame = text(&draw(&mut board, 120, 40));
    assert!(
        frame.contains("3 repositories"),
        "F-09: the count is missing:\n{frame}"
    );
    assert!(
        !frame.contains("tea…") && !frame.contains("team/websit…"),
        "F-09: a repository name was cut with an ellipsis:\n{frame}"
    );
    assert!(
        frame.contains("team/scratch-mei") && frame.contains("team/website"),
        "F-09: whole repository names are missing:\n{frame}"
    );
}

#[test]
fn the_box_edges_are_at_least_three_to_one() {
    let mut board = Board::new(ready(1));
    let buffer = draw(&mut board, 80, 24);
    // The outer frame's top-left corner is the product's edge colour.
    let edge = buffer.cell((0, 0)).expect("the frame corner").fg;
    assert_eq!(
        edge,
        Color::Rgb(91, 107, 129),
        "F-12: the box edge is the old low-contrast grey"
    );
}

#[test]
fn the_password_row_offers_a_copy_route() {
    let mut board = Board::new(ready(1));
    press(&mut board, KeyCode::Char('f'));
    let frame = text(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("c copies") && frame.contains("c copy"),
        "F-13: the password page offers no copy route:\n{frame}"
    );
}

#[test]
fn the_app_ini_hides_the_footer_timing() {
    let paths = InstancePaths::under(Path::new("/tmp/fx-r33-app-ini"));
    let secrets = InstanceSecrets::generate().expect("secrets");
    let text = bundled_forgejo::app_ini_text(&paths, 3000, "tester", &secrets);
    assert!(
        text.contains("[other]\nSHOW_FOOTER_TEMPLATE_LOAD_TIME = false"),
        "U10: the generated app.ini keeps the footer timing:\n{text}"
    );
    let old = "[other]\nSHOW_FOOTER_TEMPLATE_LOAD_TIME = true\n";
    let updated = bundled_forgejo::pinned_app_ini(old);
    assert!(
        updated.contains("SHOW_FOOTER_TEMPLATE_LOAD_TIME = false"),
        "U10: an existing data directory keeps the footer timing:\n{updated}"
    );
}

// ---- a whole run, for U8 and U9 -------------------------------------------

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-fx-r33-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the scratch root");
    root
}

fn prepare(root: &Path, endpoint: &str) -> PathBuf {
    let config = root.join("agents").join("scribe");
    let skills = config.join("skills");
    let workspace = config.join("workspace");
    fs::create_dir_all(&skills).expect("skills");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(config.join("instruction.md"), "Check the pull request.\n").expect("instruction");
    let credentials = root.join("credentials.toml");
    gewu_multiverse::mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-1");
    fs::create_dir_all(&run_dir).expect("run directory");
    RunPlan {
        run_id: "run-1".to_owned(),
        agent: "scribe".to_owned(),
        workspace,
        instruction: config.join("instruction.md"),
        skills_dir: skills,
        endpoint: endpoint.to_owned(),
        model: "example-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 10,
        confinement: "none".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan is written");
    run_dir
}

fn tool_call(command: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "shell",
                        "arguments": serde_json::json!({"command": command}).to_string()}}]}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}
    })
    .to_string()
}

fn final_reply(text: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}
    })
    .to_string()
}

#[test]
fn every_run_log_line_ends_before_the_next_timestamp() {
    let root = scratch_root("log-newline");
    let model = Stub::start(vec![
        StubReply::json(tool_call("printf http://example.test/pulls/3")),
        StubReply::json(final_reply("The pull request was checked.")),
    ]);
    let run_dir = prepare(&root, &model.url());
    let _ = agent_run::execute(&run_dir);
    let log = fs::read_to_string(run_dir.join(gewu_multiverse::run::LOG_FILE))
        .expect("the run log reads");
    assert!(
        log.contains("http://example.test/pulls/3\n"),
        "U8: the tool's URL is glued to the next log line:\n{log}"
    );
    assert!(
        !log.contains("pulls/320"),
        "U8: the next timestamp was written onto the URL:\n{log}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_model_down_reason_ends_on_a_word_naming_the_errno() {
    let root = scratch_root("model-down");
    let run_dir = prepare(&root, "http://127.0.0.1:1/v1/chat/completions");
    let _ = agent_run::execute(&run_dir);
    let reason = transcript::terminal_record(&run_dir)
        .and_then(|record| match record {
            transcript::Record::RunFinished { reason, .. } => reason,
            _ => None,
        })
        .expect("the failed run's reason");
    assert!(
        reason.contains("Connection refused (errno 111)"),
        "U9: the model-down reason does not end on the errno word: {reason}"
    );
    assert!(
        !reason.contains("os error"),
        "U9: the transport's `os error` wording reached the card: {reason}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_card_summary_prefers_the_newest_run_that_did_work() {
    let root = scratch_root("last-work");
    let layout = Layout::rooted_at(root.join("config")).expect("the layout");
    layout.ensure_root().expect("the config root");
    let mut settings = Settings {
        terms_acknowledged: true,
        terms_version: gewu_multiverse::engine::TERMS_VERSION,
        ..Settings::default()
    };
    settings.endpoint.url = "http://127.0.0.1:9/v1/chat/completions".to_owned();
    settings.endpoint.model = "demo-model".to_owned();
    settings
        .add_agent("agent-1", Desired::Stopped)
        .expect("the agent");
    let paths = layout.ensure_agent("agent-1").expect("the agent paths");
    fs::write(paths.instruction(), "check the pull request\n").expect("instruction");
    settings
        .save(&gewu_multiverse::engine::settings::path_in(layout.root()))
        .expect("settings");
    let state = paths.state_dir();
    let older = state.join("run-1700000000000-0001");
    fs::create_dir_all(&older).expect("the older run");
    let mut transcript = transcript::Transcript::open(&older).expect("the older transcript");
    transcript
        .append(&Record::forgejo_result(
            1,
            1,
            gewu_multiverse::forgejo_tools::COMMENT,
            Some("team/workspace"),
            Some(1),
            "ok",
            None,
            Some("commented"),
        ))
        .expect("the comment record");
    transcript
        .append_terminal_for_test(&gewu_multiverse::run::terminal_record_for_test(
            "run-1700000000000-0001",
            RunOutcome::Succeeded,
            Some(0),
            1_000,
            Some("I fixed the flaky test".to_owned()),
        ))
        .expect("the older finish");
    let newer = state.join("run-1700000000000-0002");
    fs::create_dir_all(&newer).expect("the newer run");
    let mut transcript = transcript::Transcript::open(&newer).expect("the newer transcript");
    transcript
        .append_terminal_for_test(&gewu_multiverse::run::terminal_record_for_test(
            "run-1700000000000-0002",
            RunOutcome::NoWork,
            Some(0),
            500,
            Some("The review is already addressed".to_owned()),
        ))
        .expect("the newer finish");

    let mut options = EngineOptions::new(layout, root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let engine = engine::open_with(options).expect("the engine opens");
    let mut board = Board::new(engine);
    press(&mut board, KeyCode::Enter);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(
        frame.contains("last completed work: comment on #1"),
        "F-10: the card did not keep the newest completed work:\n{frame}"
    );
    assert!(
        !frame.contains("result: The review is already addressed"),
        "F-10: a no-op's reason was presented as the work result:\n{frame}"
    );
    let _ = fs::remove_dir_all(&root);
}
