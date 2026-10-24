//! fx-r34 captures. Run with `GEWU_CAPTURE_DIR` set to write the review pictures:
//! the board at every size and agent count, one agent panel per run mark, the model
//! form, the warnings, the upgrade screen and the quit warning. The test is a no-op
//! in a normal suite.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine::settings::Settings;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, Provisioning, SessionEnd, SessionState, UpgradeView,
};
use gewu_multiverse::paths::Layout;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

struct Fixed(BoardView);

impl EngineApi for Fixed {
    fn board(&mut self) -> BoardView {
        self.0.clone()
    }
    fn send(&mut self, _command: Command) -> Result<(), String> {
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
                program: "builtin".to_owned(),
                program_label: "built-in engine".to_owned(),
            unknown_program: None,
                program_installed: true,
                program_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                protocol: "chat-completions".to_owned(),
                base_url: "https://model.example/v1".to_owned(),
                model: "demo-model".to_owned(),
                key_present: true,
                program_error: None,
                active_program: None,
                next_program: None,
                codex_sandbox: None,
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
        programs: Vec::new(),
        endpoint: EndpointView {
            configured: true,
            url: "https://model.example/v1/chat/completions".to_owned(),
            base_url: "https://model.example/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
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

fn save(name: &str, board: &mut Board, width: u16, height: u16) {
    let Ok(dir) = std::env::var("GEWU_CAPTURE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    fs::create_dir_all(&dir).expect("capture dir");
    let buffer = draw(board, width, height);
    fs::write(dir.join(format!("{name}.txt")), text(&buffer)).expect("capture text");
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    fs::write(
        dir.join(format!("{name}.meta.txt")),
        format!(
            "size {width}x{height}\nscreen {:?}\nutc-unix {stamp}\n",
            board.screen()
        ),
    )
    .expect("capture meta");
}

#[test]
fn capture_the_board_at_every_size_and_count() {
    if std::env::var_os("GEWU_CAPTURE_DIR").is_none() {
        return;
    }
    for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
        for count in [1, 3, 4, 7, 12] {
            let mut board = Board::new(Fixed(view(count)));
            save(
                &format!("board-{width}x{height}-{count}-agents"),
                &mut board,
                width,
                height,
            );
        }
    }
    // The newest failed run, where the card's bottom border is tightest.
    for (width, height, count) in [(72, 18, 3), (80, 24, 7)] {
        let mut failed = view(count);
        failed.agents[0].session = SessionState::Idle;
        failed.agents[0].recent = vec![SessionEnd::Failed {
            reason: "the model call failed (server_error): the model answered HTTP 500".to_owned(),
        }];
        let mut board = Board::new(Fixed(failed));
        save(
            &format!("board-{width}x{height}-{count}-failed"),
            &mut board,
            width,
            height,
        );
    }
}

#[test]
fn capture_one_agent_panel_per_mark() {
    if std::env::var_os("GEWU_CAPTURE_DIR").is_none() {
        return;
    }
    let marks = [
        ("ok", SessionEnd::Succeeded),
        (
            "refused",
            SessionEnd::Refused {
                reason: "write refused: HTTP 403".to_owned(),
            },
        ),
        (
            "no-work",
            SessionEnd::NoWork {
                reason: "nothing new to add".to_owned(),
            },
        ),
        (
            "failed",
            SessionEnd::Failed {
                reason: "the model call failed (server_error): the model answered HTTP 500"
                    .to_owned(),
            },
        ),
        (
            "cmd",
            SessionEnd::Warned {
                reason: "exit 2: make: *** No rule to make target 'test'".to_owned(),
            },
        ),
        ("stopped", SessionEnd::Stopped),
        ("timed-out", SessionEnd::TimedOut),
    ];
    for (name, mark) in marks {
        let mut board_view = view(1);
        board_view.agents[0].session = SessionState::Idle;
        board_view.agents[0].recent = vec![mark.clone()];
        board_view.agents[0].last_result = Some(match &mark {
            SessionEnd::Refused { reason }
            | SessionEnd::NoWork { reason }
            | SessionEnd::Warned { reason } => reason.clone(),
            SessionEnd::Failed { reason } => reason.clone(),
            _ => "the run ended".to_owned(),
        });
        let mut board = Board::new(Fixed(board_view));
        board.on_key(KeyEvent::from(KeyCode::Enter));
        save(&format!("agent-panel-{name}"), &mut board, 120, 40);
    }
}

#[test]
fn capture_the_model_form_warnings_upgrade_and_quit_warning() {
    if std::env::var_os("GEWU_CAPTURE_DIR").is_none() {
        return;
    }
    // The model form with a filled address and one saved key.
    let mut model_view = view(0);
    model_view.terms_acknowledged = true;
    model_view.endpoint = EndpointView {
        configured: false,
        url: "https://model.example/v1/chat/completions".to_owned(),
        base_url: "https://model.example/v1".to_owned(),
        protocol: "chat-completions".to_owned(),
        model: "demo-model".to_owned(),
        key_present: true,
        test: EndpointTest::Passed {
            answer: "the model answered".to_owned(),
        },
    };
    let mut model = Board::new(Fixed(model_view));
    save("model-form-80x24", &mut model, 80, 24);

    // The warnings.
    let mut terms = Board::new(FakeEngine::new());
    save("warnings-80x24", &mut terms, 80, 24);

    // The upgrade screen.
    let mut upgrade_view = view(3);
    upgrade_view.upgrade = Some(UpgradeView {
        from_version: "0.1.32".to_owned(),
        to_version: "0.1.33".to_owned(),
        settings: true,
        agents: 3,
        instructions: 3,
        runs: 19,
        ignored_config: false,
        release_url: format!(
            "https://github.com/Callo42/gewu-multiverse/releases/tag/v{}",
            env!("CARGO_PKG_VERSION")
        ),
        whats_new:
            "run marks name what was observed; an ordinary earlier run is stopped and its cleanup checked before the board reopens"
                .to_owned(),
        warnings_changed: true,
    });
    let mut upgrade = Board::new(Fixed(upgrade_view));
    save("upgrade-80x24", &mut upgrade, 80, 24);

    // The quit warning with one running and one blocked agent.
    let mut quit_view = view(2);
    quit_view.agents[0].desired = Desired::Started;
    quit_view.agents[0].session = SessionState::Running {
        number: 4,
        since: SystemTime::now(),
    };
    quit_view.agents[1].desired = Desired::Started;
    quit_view.agents[1].session = SessionState::Blocked {
        reason: "no instruction".to_owned(),
    };
    let mut quit = Board::new(Fixed(quit_view));
    quit.on_key(KeyEvent::from(KeyCode::Char('q')));
    save("quit-warning-80x24", &mut quit, 80, 24);

    // The warning with every agent running, at both floor sizes.
    for (width, height) in [(72, 18), (80, 24)] {
        for count in [7, 12] {
            let mut many = view(count);
            for agent in &mut many.agents {
                agent.desired = Desired::Started;
                agent.session = SessionState::Running {
                    number: 1,
                    since: SystemTime::now(),
                };
            }
            let mut quit = Board::new(Fixed(many));
            quit.on_key(KeyEvent::from(KeyCode::Char('q')));
            save(
                &format!("quit-warning-{width}x{height}-{count}-running"),
                &mut quit,
                width,
                height,
            );
        }
    }
}

/// The review-2 matrix: 1, 4, 7 and 12 running agents at every supported size,
/// with and without a repository-event row and with and without a status
/// sentence. The frames are the evidence for the footer-geometry guard.
#[test]
fn capture_the_quit_warning_matrix() {
    if std::env::var_os("GEWU_CAPTURE_DIR").is_none() {
        return;
    }
    for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
        for count in [1, 4, 7, 12] {
            for event in [false, true] {
                for status in [false, true] {
                    let mut many = view(count);
                    for agent in &mut many.agents {
                        agent.desired = Desired::Started;
                        agent.session = SessionState::Running {
                            number: 1,
                            since: SystemTime::now(),
                        };
                    }
                    if !event {
                        many.forgejo.events.clear();
                    }
                    if status {
                        many.notice = Some("copied repository URL".to_owned());
                    }
                    let mut quit = Board::new(Fixed(many));
                    quit.on_key(KeyEvent::from(KeyCode::Char('q')));
                    save(
                        &format!(
                            "quit-{width}x{height}-{count}-{}-{}",
                            if event { "event" } else { "no-event" },
                            if status { "status" } else { "no-status" },
                        ),
                        &mut quit,
                        width,
                        height,
                    );
                }
            }
        }
    }
}

#[test]
fn capture_the_real_upgrade_with_changed_warnings() {
    if std::env::var_os("GEWU_CAPTURE_DIR").is_none() {
        return;
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "gewu-fx-r34-upgrade-capture-{}-{stamp}",
        std::process::id()
    ));
    let layout = Layout::rooted_at(root.clone()).expect("an absolute layout");
    layout.ensure_root().expect("the root");
    gewu_multiverse::engine::last_seen::write(&root, "0.1.32").expect("the old version");
    let mut settings = Settings {
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
    let mut options = EngineOptions::new(layout, root.join("data")).expect("options");
    options.auto_start_forgejo = false;
    let engine = engine::open_with(options).expect("the engine");
    let mut board = Board::new(engine);
    save("upgrade-real-upgrade-80x24", &mut board, 80, 24);
    board.on_key(KeyEvent::from(KeyCode::Enter));
    save("upgrade-real-warnings-80x24", &mut board, 80, 24);
    for _ in 0..12 {
        board.on_key(KeyEvent::from(KeyCode::Char('y')));
        if board.screen() == gewu_multiverse::board::Screen::Home {
            break;
        }
        board.on_key(KeyEvent::from(KeyCode::Down));
        let _ = draw(&mut board, 80, 24);
    }
    assert_eq!(
        board.screen(),
        gewu_multiverse::board::Screen::Home,
        "the real upgrade flow did not return to the board"
    );
    save("upgrade-real-board-80x24", &mut board, 80, 24);
    let _ = fs::remove_dir_all(&root);
}
