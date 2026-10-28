//! Wave 37 cause regressions: the board's model address, per-agent test, Program
//! preview, retrying 503, upgrade/recovery words and the small layout repairs.
//!
//! These are deterministic board tests over one `BoardView`. The real-PTY captures
//! from the packaged release and the native loopback runs are separate evidence.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::{Board, Screen};
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, ProgramView, Provisioning, SessionEnd, SessionState, UpgradeAgent, UpgradeView,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

struct ViewEngine {
    view: BoardView,
    sent: Arc<Mutex<Vec<Command>>>,
}

impl EngineApi for ViewEngine {
    fn board(&mut self) -> BoardView {
        self.view.clone()
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
        if let Command::TestAgentModel { .. } = &command {
            // Mimic the engine's background test result so the form's own test
            // line can be asserted without a network call.
            self.view.agents[0].model_test = EndpointTest::Failed {
                reason: "the provider refused the API key (HTTP 401): Invalid API key provided — check the key"
                    .to_owned(),
            };
        }
        self.sent.lock().expect("sent").push(command);
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

fn program(id: &str, installed: bool) -> ProgramView {
    ProgramView {
        id: id.to_owned(),
        label: match id {
            "pi" => "Pi",
            "codex" => "Codex",
            "dsh" => "DeepSeek Harness",
            "opencode" => "OpenCode",
            _ => "built-in engine",
        }
        .to_owned(),
        version: "1.0.0".to_owned(),
        installed_version: installed.then(|| "1.0.0".to_owned()),
        installed,
        license: "MIT".to_owned(),
        download_mb: 10,
        prerequisite: None,
        prerequisite_met: true,
        installing: None,
        installing_phase: None,
        error: None,
    }
}

fn agent(name: &str, program_id: &str) -> AgentView {
    AgentView {
        name: name.to_owned(),
        login: format!("gewu-{name}"),
        provisioning: Provisioning::Ready,
        desired: Desired::Stopped,
        session: SessionState::Idle,
        sessions_since_start: 0,
        runs_total: 3,
        recent: vec![SessionEnd::Succeeded],
        instruction_chars: 12,
        instruction_first_line: Some("do the work".to_owned()),
        instruction: Some("do the work\n".to_owned()),
        instruction_lines: 1,
        activity: None,
        activity_at: None,
        last_error: None,
        last_result: Some("done".to_owned()),
        last_failure: None,
        leftover: None,
        current_tool: None,
        current_tool_seconds: 0,
        retrying: None,
        last_work: vec!["comment on #1".to_owned()],
        pull_states: Vec::new(),
        no_assigned_work: false,
        last_work_result: Some("done".to_owned()),
        last_result_is_work: true,
        program: program_id.to_owned(),
        program_label: if program_id == "pi" {
            "Pi"
        } else {
            "built-in engine"
        }
        .to_owned(),
        unknown_program: None,
        program_installed: true,
        program_version: Some("1.0.0".to_owned()),
        protocol: "chat-completions".to_owned(),
        base_url: "https://model.example/v1".to_owned(),
        model: "demo-model".to_owned(),
        key_present: true,
        model_test: EndpointTest::NotRun,
        program_error: None,
        active_program: None,
        next_program: None,
        codex_sandbox: None,
    }
}

fn view(agents: Vec<AgentView>, programs: Vec<ProgramView>) -> BoardView {
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
            repositories: 1,
            repository_names: vec!["team/workspace".to_owned()],
            events: Vec::new(),
            admin_login_kept: false,
        },
        agents,
        endpoint: EndpointView {
            configured: true,
            url: "https://model.example/v1/chat/completions".to_owned(),
            base_url: "https://model.example/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "demo-model".to_owned(),
            key_present: true,
            test: EndpointTest::NotRun,
        },
        programs,
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

fn installed_programs() -> Vec<ProgramView> {
    ["pi", "codex", "dsh", "opencode"]
        .into_iter()
        .map(|id| program(id, true))
        .collect()
}

fn board(view: BoardView) -> (Board, Arc<Mutex<Vec<Command>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    (
        Board::new(ViewEngine {
            view,
            sent: Arc::clone(&sent),
        }),
        sent,
    )
}

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn press_ctrl(board: &mut Board, ch: char) {
    board.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL));
}

fn type_text(board: &mut Board, text: &str) {
    for ch in text.chars() {
        press(board, KeyCode::Char(ch));
    }
}

fn frame_text(board: &mut Board, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| board.draw(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn open_agent_model(board: &mut Board) {
    press(board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);
    press(board, KeyCode::Char('m'));
    assert_eq!(board.screen(), Screen::Model);
}

fn open_program(board: &mut Board) {
    press(board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);
    press(board, KeyCode::Char('p'));
    assert_eq!(board.screen(), Screen::Program);
}

#[test]
fn c2_a_matching_full_route_is_reduced_to_its_base() {
    let (mut board, sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_agent_model(&mut board);
    press(&mut board, KeyCode::Char('e'));
    press_ctrl(&mut board, 'u');
    type_text(&mut board, "https://model.example/v1/responses");
    press(&mut board, KeyCode::Enter);
    // `⏎` committed the field and moved to the model; move back to the address and
    // choose the protocol the route names.
    press(&mut board, KeyCode::Up);
    press(&mut board, KeyCode::Right);
    press(&mut board, KeyCode::Enter);

    let saved = sent
        .lock()
        .expect("sent")
        .iter()
        .find_map(|command| match command {
            Command::SetAgentModel {
                base_url, protocol, ..
            } => Some((base_url.clone(), protocol.clone())),
            _ => None,
        })
        .expect("the connection is saved");
    assert_eq!(saved.0, "https://model.example/v1", "{saved:?}");
    assert_eq!(saved.1, "responses", "{saved:?}");
}

#[test]
fn c2_a_mismatched_route_names_the_protocol_it_belongs_to() {
    let (mut board, sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_agent_model(&mut board);
    press(&mut board, KeyCode::Char('e'));
    press_ctrl(&mut board, 'u');
    type_text(&mut board, "https://model.example/v1/responses");
    press(&mut board, KeyCode::Enter);
    // The protocol is still Chat Completions; Enter saves and must refuse.
    press(&mut board, KeyCode::Enter);
    let notice = board.notice().unwrap_or_default();
    assert!(notice.contains("OpenAI Responses"), "{notice}");
    assert!(
        !sent
            .lock()
            .expect("sent")
            .iter()
            .any(|command| matches!(command, Command::SetAgentModel { .. }))
    );
}

#[test]
fn c3_t_runs_the_agents_own_test_and_shows_the_provider_action() {
    let (mut board, sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_agent_model(&mut board);
    press(&mut board, KeyCode::Char('t'));
    let commands = sent.lock().expect("sent").clone();
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Command::SetAgentModel { .. })),
        "{commands:?}"
    );
    assert!(
        commands.iter().any(|command| matches!(
            command,
            Command::TestAgentModel { name } if name == "agent-1"
        )),
        "{commands:?}"
    );
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("HTTP 401"), "{frame}");
    assert!(frame.contains("check the key"), "{frame}");
}

#[test]
fn c3_the_first_enter_says_how_to_save() {
    let (mut board, _sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_agent_model(&mut board);
    press(&mut board, KeyCode::Char('e'));
    press(&mut board, KeyCode::Enter);
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("press ⏎ again to save"), "{frame}");
    assert!(!frame.contains("Start is the test"), "{frame}");
}

#[test]
fn c3_the_403_sentence_and_its_action_are_whole_on_the_agent_screen() {
    let mut broken = agent("agent-1", "pi");
    broken.session = SessionState::Idle;
    broken.desired = Desired::Stopped;
    broken.recent = vec![SessionEnd::Failed {
        reason: "the model call failed (bad_key): HTTP 403: You do not have permission to use this model (permission_error) — check the key"
            .to_owned(),
    }];
    broken.last_error = Some(
        "the model call failed (bad_key): HTTP 403: You do not have permission to use this model (permission_error)"
            .to_owned(),
    );
    let (mut board, _sent) = board(view(vec![broken], installed_programs()));
    press(&mut board, KeyCode::Enter);
    let frame = frame_text(&mut board, 80, 24);
    let flat = frame
        .replace(['│', '╭', '╮', '╰', '╯', '─', '├', '┤', '┬', '┴', '┼'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        flat.contains("You do not have permission to use this model"),
        "the provider's sentence is cut:\n{frame}"
    );
    assert!(
        flat.contains("access denied") && flat.contains("check the model and its access"),
        "a 403 is still told as a key problem:\n{frame}"
    );
    assert!(!flat.contains("check the key"), "{frame}");
}

#[test]
fn c3_an_empty_key_clears_the_bullets_and_the_footer_is_not_cut_midword() {
    let (mut board, sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_agent_model(&mut board);
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Down);
    press_ctrl(&mut board, 'u');
    let frame = frame_text(&mut board, 72, 18);
    assert!(!frame.contains("••••••••"), "{frame}");
    assert!(!frame.contains("Start is the t"), "{frame}");
    assert!(
        frame.contains("t test") && frame.contains("esc back"),
        "{frame}"
    );
    press(&mut board, KeyCode::Enter);
    assert!(
        sent.lock().expect("sent").iter().any(|command| {
            matches!(command, Command::SetAgentModel { key, .. } if key.as_deref() == Some(""))
        }),
        "the emptied key was refused instead of clearing the stored one"
    );
}

#[test]
fn c4_next_follows_the_highlight_and_the_new_programs_own_connection() {
    let (mut board, sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    open_program(&mut board);
    let frame = frame_text(&mut board, 120, 24);
    assert!(
        frame.contains("latest run:"),
        "the latest word is not named:\n{frame}"
    );
    assert!(frame.contains("next run: Pi"), "{frame}");

    // Pi -> Codex: Codex only uses OpenAI Responses.
    press(&mut board, KeyCode::Down);
    let frame = frame_text(&mut board, 120, 24);
    assert!(frame.contains("next run: Codex"), "{frame}");
    assert!(frame.contains("connection: OpenAI Responses"), "{frame}");

    // Codex -> DeepSeek Harness: its own default is Chat Completions, not the
    // previous program's Responses.
    press(&mut board, KeyCode::Down);
    let frame = frame_text(&mut board, 120, 24);
    assert!(frame.contains("next run: DeepSeek Harness"), "{frame}");
    assert!(frame.contains("connection: Chat Completions"), "{frame}");
    assert!(
        !sent
            .lock()
            .expect("sent")
            .iter()
            .any(|command| matches!(command, Command::SetAgentProgram { .. }))
    );

    // The Codex line uses the same two-press action as the Agent screen.
    press(&mut board, KeyCode::Up);
    let frame = frame_text(&mut board, 120, 24);
    assert!(frame.contains("press f twice"), "{frame}");
    assert!(frame.contains("removes Codex's own sandbox"), "{frame}");
}

#[test]
fn c4_choosing_an_uninstalled_program_offers_the_install_first() {
    let mut programs = installed_programs();
    programs[0].installed = false;
    programs[0].installed_version = None;
    let (mut board, sent) = board(view(vec![agent("agent-1", "builtin")], programs));
    open_program(&mut board);
    // Built-in, then Pi (not installed).
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Program);
    assert!(
        board.notice().unwrap_or_default().contains("not installed"),
        "{:?}",
        board.notice()
    );
    assert!(
        !sent
            .lock()
            .expect("sent")
            .iter()
            .any(|command| matches!(command, Command::SetAgentProgram { .. }))
    );
}

#[test]
fn c5_a_live_retry_names_the_503_without_a_failed_mark() {
    let mut worker = agent("agent-1", "pi");
    worker.desired = Desired::Started;
    worker.session = SessionState::Running {
        number: 1,
        since: SystemTime::now(),
    };
    worker.retrying = Some("the provider answered HTTP 503: temporarily unavailable".to_owned());
    let (mut board, _sent) = board(view(vec![worker], installed_programs()));
    let frame = frame_text(&mut board, 80, 24);
    assert!(frame.contains("HTTP 503"), "{frame}");
    assert!(frame.contains("retrying"), "{frame}");
    assert!(!frame.contains("✗ failed"), "{frame}");
}

#[test]
fn c6_upgrade_lists_the_kept_programs_and_models() {
    let mut board_view = view(vec![agent("agent-1", "pi")], installed_programs());
    board_view.upgrade = Some(UpgradeView {
        from_version: "0.1.38".to_owned(),
        to_version: "0.1.39".to_owned(),
        settings: true,
        agents: 1,
        instructions: 1,
        runs: 3,
        ignored_config: false,
        kept_agents: vec![UpgradeAgent {
            name: "agent-1".to_owned(),
            program: "Pi".to_owned(),
            model: "demo-model".to_owned(),
            protocol: "Chat Completions".to_owned(),
        }],
        programs: 1,
        release_url: "https://github.com/Callo42/gewu-multiverse/releases/tag/v0.1.39".to_owned(),
        whats_new: "run marks name what was observed".to_owned(),
        warnings_changed: false,
    });
    let (mut board, _sent) = board(board_view);
    assert_eq!(board.screen(), Screen::Upgrade);
    let frame = frame_text(&mut board, 80, 24);
    assert!(
        frame.contains("kept: agent-1 · Pi · demo-model · Chat Completions"),
        "{frame}"
    );
    assert!(frame.contains("1 installed program"), "{frame}");
}

#[test]
fn c7_the_error_card_names_the_program_once_and_the_wire_keeps_a_blank_edge() {
    let mut broken = agent("agent-1", "pi");
    broken.session = SessionState::Idle;
    broken.desired = Desired::Stopped;
    broken.recent = vec![SessionEnd::Failed {
        reason: "the model call failed (bad_key): HTTP 403: You do not have permission to use this model"
            .to_owned(),
    }];
    broken.last_error = Some("HTTP 403: You do not have permission to use this model".to_owned());
    let (mut one, _sent) = board(view(vec![broken], installed_programs()));
    let one_frame = frame_text(&mut one, 80, 24);
    assert_eq!(one_frame.matches("Pi").count(), 1, "{one_frame}");

    let agents: Vec<AgentView> = (1..=3)
        .map(|n| agent(&format!("agent-{n}"), "pi"))
        .collect();
    let (mut three, _sent) = board(view(agents, installed_programs()));
    let frame = frame_text(&mut three, 80, 24);
    assert!(
        !frame.contains("│╭"),
        "a connector touches both card edges:\n{frame}"
    );
    assert!(
        !frame.contains("╮│"),
        "a connector touches both card edges:\n{frame}"
    );
}

#[test]
fn c7_a_large_frame_starts_near_the_top_and_the_install_wait_has_an_elapsed_fact() {
    let (mut top, _sent) = board(view(vec![agent("agent-1", "pi")], installed_programs()));
    let frame = frame_text(&mut top, 120, 40);
    let forgejo_row = frame
        .lines()
        .position(|line| line.contains("FORGEJO"))
        .expect("the Forgejo box");
    assert!(
        forgejo_row <= 3,
        "the picture is centred in emptiness:\n{frame}"
    );

    let mut programs = installed_programs();
    programs[0].installing = Some(None);
    programs[0].installing_phase = Some("installing packages · 12s".to_owned());
    let (mut install, _sent) = board(view(vec![agent("agent-1", "pi")], programs));
    open_program(&mut install);
    let frame = frame_text(&mut install, 80, 24);
    assert!(frame.contains("installing packages · 12s"), "{frame}");
}
