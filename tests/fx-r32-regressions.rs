//! Guards for the wave-31 findings that can be shown failing on the base commit.
//!
//! This file deliberately uses only APIs that existed at `04aa61b`, so the same file
//! can be copied into a base worktree and run there: each assertion names the
//! behaviour the base lacked.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use gewu_multiverse::board::{Board, FakeEngine, Screen};
use gewu_multiverse::credentials::Secret;
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi};
use gewu_multiverse::model::{self, ModelConfig};
use gewu_multiverse::run;
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::from(code));
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

fn frame(board: &mut Board, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    text(terminal.backend().buffer())
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
    engine
        .send(Command::CreateAgents { count })
        .expect("agents are created");
    engine.send(Command::StartForgejo).expect("Forgejo starts");
    for _ in 0..160 {
        engine.tick();
    }
    let _ = engine.board();
    engine
}

fn picker() -> Board {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    Board::new(engine)
}

/// An engine whose view carries the facts the walk found missing, using only the
/// base `BoardView` fields.
struct Rich(FakeEngine);

impl EngineApi for Rich {
    fn board(&mut self) -> BoardView {
        let mut view = self.0.board();
        view.forgejo.repository_names = vec![
            "team/workspace-long-name".to_owned(),
            "team/website-long-name".to_owned(),
        ];
        view.forgejo.admin_login = "mei".to_owned();
        view.forgejo.admin_login_kept = true;
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

fn config(endpoint: String) -> ModelConfig {
    ModelConfig {
        endpoint,
        model: "demo-model".to_owned(),
        api_key: Secret::new("sk-test"),
        timeout: Duration::from_secs(2),
    }
}

#[test]
fn a_base_url_is_refused_before_a_request() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"Add /v1 to the base URL."}"#),
        StubReply::status(404, "no models"),
    ]);
    let base = stub.url().replace("/v1/chat/completions", "");
    let error = model::chat(&config(base), &[model::Message::user("hi")], &[], None)
        .expect_err("a base URL is refused");
    assert_eq!(error.class(), "bad_address");
    assert_eq!(stub.requests().len(), 0, "a request was sent");
}

#[test]
fn a_provider_404_keeps_the_providers_own_first_clause() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"Add /v1 to the base URL."}"#),
        StubReply::status(404, "no models"),
    ]);
    let endpoint = stub
        .url()
        .replace("/v1/chat/completions", "/chat/completions");
    let error = model::chat(&config(endpoint), &[model::Message::user("hi")], &[], None)
        .expect_err("the provider refuses the path");
    let words = error.to_string();
    assert!(words.contains("Add /v1 to the base URL"), "{words}");
    assert!(words.contains("404"), "{words}");
}

#[test]
fn the_forgejo_team_link_is_org_not_orgs() {
    let mut board = Board::new(Rich(ready(3)));
    press(&mut board, KeyCode::Char('f'));
    let frame = frame(&mut board, 120, 40);
    assert!(frame.contains("/org/team/teams/agents"), "{frame}");
    assert!(!frame.contains("/orgs/team/teams/agents"), "{frame}");
}

#[test]
fn the_floor_forgejo_page_keeps_version_organisation_and_team_page() {
    let mut board = Board::new(Rich(ready(3)));
    press(&mut board, KeyCode::Char('f'));
    let frame = frame(&mut board, 72, 18);
    for wanted in [
        "17.0.0",
        "organisation",
        "team page",
        "/org/team/teams/agents",
    ] {
        assert!(frame.contains(wanted), "{wanted} missing:\n{frame}");
    }
}

#[test]
fn the_agent_count_screen_labels_itself_a_preview() {
    let board = picker();
    assert_eq!(board.screen(), Screen::Picker);
    let mut board = board;
    let frame = frame(&mut board, 80, 24);
    assert!(frame.contains("preview"), "{frame}");
    assert!(!frame.contains("● running"), "{frame}");
}

#[test]
fn the_removal_confirmation_names_its_keys() {
    let mut board = Board::new(ready(2));
    press(&mut board, KeyCode::Char('-'));
    let frame = frame(&mut board, 80, 24);
    assert!(frame.contains("y remove   n keep"), "{frame}");
}

#[test]
fn the_selected_card_has_a_non_colour_mark() {
    let mut board = Board::new(ready(1));
    let frame = frame(&mut board, 120, 40);
    assert!(frame.contains("▸ agent-1"), "{frame}");
}

#[test]
fn the_password_row_says_ten_seconds() {
    let mut board = Board::new(ready(1));
    press(&mut board, KeyCode::Char('f'));
    let frame = frame(&mut board, 80, 24);
    assert!(frame.contains("shown for 10 s"), "{frame}");
}

#[test]
fn the_shell_description_does_not_call_a_confined_run_unconfined() {
    let shell = &model::tools()[0].function;
    assert!(
        !shell.description.contains("not confinement"),
        "{}",
        shell.description
    );
}

#[test]
fn the_terms_name_no_mechanism() {
    for term in gewu_multiverse::engine::TERMS {
        let lower = term.to_ascii_lowercase();
        for mechanism in ["landlock", "bwrap", "unshare", "gewu_confinement"] {
            assert!(!lower.contains(mechanism), "{term}");
        }
    }
}

#[test]
fn the_work_block_types_a_pull_request_and_a_comment() {
    let root = std::env::temp_dir().join(format!(
        "gewu-regress-work-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    let run_dir = root.join("run-1700000000000-0001");
    fs::create_dir_all(&run_dir).expect("the run directory");
    let mut transcript = Transcript::open(&run_dir).expect("transcript");
    transcript
        .append(&Record::forgejo_call(
            1,
            1,
            "forgejo_read_issue_or_pull",
            Some("team/website"),
            Some(5),
            "no body",
        ))
        .expect("call");
    transcript
        .append(&Record::forgejo_result(
            1,
            2,
            "forgejo_open_pull_request",
            Some("team/website"),
            Some(5),
            "ok",
            Some("http://127.0.0.1:3000/team/website/pulls/5"),
            Some("opened"),
        ))
        .expect("open");
    transcript
        .append(&Record::forgejo_result(
            1,
            3,
            "forgejo_comment",
            Some("team/website"),
            Some(1),
            "ok",
            Some("http://127.0.0.1:3000/team/website/issues/1#c"),
            Some("commented"),
        ))
        .expect("comment");
    run::write_push_witness_at(&run_dir, "fix/x", "refs/remotes/origin/fix/x", None, None)
        .expect("witness");
    let lines = run::work_summary(&run_dir);
    assert_eq!(
        lines,
        vec![
            "pull request #5 from fix/x".to_owned(),
            "comment on #1".to_owned()
        ],
        "the base labels the pull request an issue or loses the rows"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_wide_window_keeps_a_long_repository_name() {
    let mut board = Board::new(Rich(ready(3)));
    let buffer = {
        let backend = TestBackend::new(200, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| board.draw(f)).expect("the frame draws");
        terminal.backend().buffer().clone()
    };
    let frame = flat(&buffer);
    assert!(frame.contains("team/workspace-long-name"), "{frame}");
    assert!(frame.contains("team/website-long-name"), "{frame}");
    assert!(!frame.contains("team/workspa…"), "{frame}");
}
