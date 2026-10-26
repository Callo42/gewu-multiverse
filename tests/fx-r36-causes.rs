//! Wave 36 cause regressions: native work is work, native program prerequisites stay
//! visible, the per-agent form keeps the first-run checks and the protocol, and the
//! board carries the program, run mark, newest work and future tense.
//!
//! These are renderer/contract tests over the same `BoardView` the engine writes.
//! The live native runs and real-PTY captures are separate evidence in `return/`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::Board;
use gewu_multiverse::board::status;
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, ProgramView, Provisioning, RunProgramView, SessionEnd, SessionState,
};
use gewu_multiverse::model::{self, ModelError};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

struct Fixed {
    view: BoardView,
    sent: Arc<Mutex<Vec<Command>>>,
}

impl EngineApi for Fixed {
    fn board(&mut self) -> BoardView {
        self.view.clone()
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
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

fn agent(number: usize) -> AgentView {
    AgentView {
        name: format!("agent-{number}"),
        login: format!("gewu-agent-{number}"),
        provisioning: Provisioning::Ready,
        desired: Desired::Stopped,
        session: SessionState::Idle,
        sessions_since_start: 1,
        runs_total: number as u64 + 1,
        recent: vec![SessionEnd::Succeeded],
        instruction_chars: 24,
        instruction_first_line: Some("work the assigned issue".to_owned()),
        instruction: Some("work the assigned issue\n".to_owned()),
        instruction_lines: 1,
        activity: None,
        activity_at: None,
        last_error: None,
        last_result: Some("I worked on the issue".to_owned()),
        last_failure: None,
        leftover: None,
        current_tool: None,
        current_tool_seconds: 0,
        retrying: None,
        last_work: vec![format!("comment on #{number}")],
        no_assigned_work: false,
        last_work_result: Some("I worked on the issue".to_owned()),
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
        model_test: EndpointTest::NotRun,
        program_error: None,
        active_program: None,
        next_program: None,
        codex_sandbox: None,
    }
}

fn view(agents: Vec<AgentView>) -> BoardView {
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
        programs: Vec::new(),
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

fn board(view: BoardView) -> (Board, Arc<Mutex<Vec<Command>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    (
        Board::new(Fixed {
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

fn text(board: &mut Board, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
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

fn open_agent(board: &mut Board) {
    press(board, KeyCode::Enter);
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Agent);
}

fn program_view(id: &str) -> ProgramView {
    ProgramView {
        id: id.to_owned(),
        label: match id {
            "dsh" => "DeepSeek Harness",
            "opencode" => "OpenCode",
            "codex" => "Codex",
            "pi" => "Pi",
            _ => "built-in engine",
        }
        .to_owned(),
        version: "0.2.1-rc.2".to_owned(),
        installed_version: None,
        installed: false,
        license: "MIT".to_owned(),
        download_mb: 753,
        prerequisite: (id == "dsh").then(|| "Node 24.9+ / npm 11.16+".to_owned()),
        prerequisite_met: id != "dsh",
        installing: None,
        installing_phase: None,
        error: None,
    }
}

// ---- C1: the native ending model counts a product-observed write as work ----

#[test]
fn c1_status_words_keep_the_provider_message_and_the_503_retry() {
    let words = status::failure_words("HTTP 401: Incorrect API key provided");
    assert!(words.contains("Incorrect API key provided"), "{words}");
    assert!(words.contains("check the key"), "{words}");
    let busy = status::failure_words("the provider answered HTTP 503: temporarily unavailable");
    assert!(busy.contains("temporarily unavailable"), "{busy}");
    assert!(busy.contains("retrying"), "{busy}");
}

#[test]
fn c1_the_model_error_keeps_the_provider_body_and_names_the_fix() {
    let words = model::failure_words(
        &ModelError::BadKey {
            status: 401,
            message: "Incorrect API key provided".to_owned(),
        },
        "demo",
    );
    assert!(words.contains("Incorrect API key provided"), "{words}");
    assert!(words.contains("check the key"), "{words}");
}

// ---- C2: the prerequisite line is persistent and exact ----

#[test]
fn c2_the_codex_sandbox_cause_and_action_stay_on_the_72x18_agent_screen() {
    let mut one = agent(1);
    one.program = "codex".to_owned();
    one.program_label = "Codex".to_owned();
    one.session = SessionState::Blocked {
        reason: "Codex's sandbox cannot start (bwrap: loopback: Failed RTM_NEWADDR) — press f twice: removes Codex's own sandbox, keeps product Landlock".to_owned(),
    };
    one.program_error = Some(match &one.session {
        SessionState::Blocked { reason } => reason.clone(),
        _ => unreachable!(),
    });
    one.recent.clear();
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    let frame = text(&mut board, 72, 18);
    assert!(frame.contains("bwrap"), "the cause is gone:\n{frame}");
    assert!(
        frame.contains("press f twice"),
        "the action is gone:\n{frame}"
    );
    assert!(
        frame.contains("product Landlock"),
        "what the opt-out gives up is gone:\n{frame}"
    );
}

#[test]
fn c2_the_dsh_versions_and_install_key_stay_on_the_72x18_agent_screen() {
    let mut one = agent(1);
    one.program = "dsh".to_owned();
    one.program_label = "DeepSeek Harness".to_owned();
    one.session = SessionState::Blocked {
        reason: "DeepSeek Harness cannot start: install Node.js 24.9+ (npm 11.16+), then press i"
            .to_owned(),
    };
    one.program_error = Some(
        "DeepSeek Harness cannot start: install Node.js 24.9+ (npm 11.16+), then press i"
            .to_owned(),
    );
    one.recent.clear();
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    let frame = text(&mut board, 72, 18);
    assert!(frame.contains("Node.js 24.9+"), "{frame}");
    assert!(frame.contains("npm 11.16+"), "{frame}");
    assert!(frame.contains("press i"), "{frame}");
}

#[test]
fn c2_the_program_screen_names_the_opt_out_without_full_access_jargon() {
    let mut one = agent(1);
    one.program = "codex".to_owned();
    one.program_label = "Codex".to_owned();
    one.codex_sandbox = Some("full-access".to_owned());
    one.active_program = Some(RunProgramView {
        program: "codex".to_owned(),
        label: "Codex".to_owned(),
        version: "0.164.0".to_owned(),
        protocol: "responses".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: "stub-codex".to_owned(),
        codex_sandbox: Some("full-access".to_owned()),
    });
    one.next_program = one.active_program.clone();
    let mut v = view(vec![one]);
    v.programs = vec![program_view("pi"), program_view("codex")];
    let (mut board, _) = board(v);
    open_agent(&mut board);
    press(&mut board, KeyCode::Char('p'));
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let frame = text(&mut board, width, height);
        assert!(
            frame.contains("sandbox off (product Landlock)"),
            "{width}x{height}: the opt-out state was not named:\n{frame}"
        );
        assert!(
            !frame.contains("full-access"),
            "{width}x{height}: full-access jargon remains:\n{frame}"
        );
    }
}

#[test]
fn c2_the_dsh_missing_row_keeps_the_versions_and_the_i_action() {
    let mut one = agent(1);
    one.program = "dsh".to_owned();
    one.program_label = "DeepSeek Harness".to_owned();
    let mut dsh = program_view("dsh");
    dsh.error = Some("Node.js 24.9 or newer and npm are needed for DeepSeek Harness".to_owned());
    dsh.prerequisite_met = false;
    let mut v = view(vec![one]);
    v.programs = vec![
        program_view("pi"),
        program_view("codex"),
        dsh,
        program_view("opencode"),
    ];
    let (mut board, _) = board(v);
    open_agent(&mut board);
    press(&mut board, KeyCode::Char('p'));
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let frame = text(&mut board, width, height);
        assert!(
            frame.contains("Node.js 24.9+ (npm 11.16+)"),
            "{width}x{height}: the exact floors are gone:\n{frame}"
        );
        assert!(
            frame.contains("then press i"),
            "{width}x{height}: the action is gone:\n{frame}"
        );
    }
}

// ---- C3: per-agent save keeps the first-run checks and shows the protocol ----

#[test]
fn c3_the_per_agent_form_shows_and_changes_the_protocol() {
    let mut one = agent(1);
    one.program = "pi".to_owned();
    one.program_label = "Pi".to_owned();
    one.protocol = "responses".to_owned();
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    press(&mut board, KeyCode::Char('m'));
    let frame = text(&mut board, 80, 24);
    assert!(frame.contains("protocol: OpenAI Responses"), "{frame}");
    assert!(frame.contains("←→ change"), "{frame}");
    press(&mut board, KeyCode::Right);
    let frame = text(&mut board, 80, 24);
    assert!(
        frame.contains("protocol: Anthropic Messages"),
        "the protocol did not cycle:\n{frame}"
    );
}

#[test]
fn c3_clearing_the_key_refuses_the_save_instead_of_saving_it_silently() {
    let one = agent(1);
    let (mut board, sent) = board(view(vec![one]));
    open_agent(&mut board);
    press(&mut board, KeyCode::Char('m'));
    // The key field is the third one, and the stored key is still present; ctrl-u
    // clears it explicitly.
    press(&mut board, KeyCode::Down);
    press(&mut board, KeyCode::Down);
    press_ctrl(&mut board, 'u');
    press(&mut board, KeyCode::Enter);
    assert!(
        sent.lock().expect("sent").is_empty(),
        "an empty key was saved: {:?}",
        sent.lock().expect("sent")
    );
    assert!(
        board
            .notice()
            .is_some_and(|notice| notice.contains("API key is empty")),
        "the form did not say the key is empty: {:?}",
        board.notice()
    );
}

// ---- C4: program, run mark, newest work, active/next and future tense ----

#[test]
fn c4_every_card_names_its_program_and_newest_run_mark_at_72_80_120() {
    let mut one = agent(1);
    one.program = "builtin".to_owned();
    one.program_label = "built-in engine".to_owned();
    let mut two = agent(2);
    two.program = "dsh".to_owned();
    two.program_label = "DeepSeek Harness".to_owned();
    two.recent = vec![SessionEnd::Warned {
        reason: "the native program finished without verified success".to_owned(),
    }];
    let mut three = agent(3);
    three.program = "opencode".to_owned();
    three.program_label = "OpenCode".to_owned();
    three.recent = vec![SessionEnd::Failed {
        reason: "HTTP 500 from the model".to_owned(),
    }];
    let mut four = agent(4);
    four.program = "pi".to_owned();
    four.program_label = "Pi".to_owned();
    let (mut board, _) = board(view(vec![one, two, three, four]));
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let frame = text(&mut board, width, height);
        for word in ["built-in", "DSH", "OpenCode", "Pi"] {
            assert!(
                frame.contains(word),
                "{width}x{height}: the card does not name {word}:\n{frame}"
            );
        }
        for mark in ['✓', '⚠', '✗'] {
            assert!(
                frame.contains(mark),
                "{width}x{height}: the newest run mark {mark} is gone:\n{frame}"
            );
        }
    }
}

#[test]
fn c4_last_work_shows_the_newest_work_not_the_run_warning() {
    let mut one = agent(1);
    one.last_work = vec![
        "comment on #2".to_owned(),
        "pull request #3 from fix".to_owned(),
    ];
    one.last_result = Some("the native program finished without verified success".to_owned());
    one.last_result_is_work = false;
    one.recent = vec![SessionEnd::Warned {
        reason: "the native program finished without verified success".to_owned(),
    }];
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    let frame = text(&mut board, 120, 40);
    assert!(frame.contains("last work: PR #3 open"), "{frame}");
    assert!(!frame.contains("last work: the native"), "{frame}");
}

#[test]
fn c4_active_and_next_are_shown_together_when_they_differ() {
    let mut one = agent(1);
    one.program = "pi".to_owned();
    one.program_label = "Pi".to_owned();
    one.session = SessionState::Running {
        number: 2,
        since: SystemTime::now(),
    };
    one.active_program = Some(RunProgramView {
        program: "pi".to_owned(),
        label: "Pi".to_owned(),
        version: "1.4.0".to_owned(),
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: "old-model".to_owned(),
        codex_sandbox: None,
    });
    one.next_program = Some(RunProgramView {
        program: "pi".to_owned(),
        label: "Pi".to_owned(),
        version: "1.4.0".to_owned(),
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: "new-model".to_owned(),
        codex_sandbox: None,
    });
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    for (width, height) in [(72, 18), (120, 40)] {
        let frame = text(&mut board, width, height);
        assert!(
            frame.contains("active Pi old-model · next Pi new-model"),
            "{width}x{height}: {frame}"
        );
    }
}

#[test]
fn c4_an_address_only_change_is_visible_in_the_agent_and_program_facts() {
    let mut one = agent(1);
    one.program = "pi".to_owned();
    one.program_label = "Pi".to_owned();
    one.session = SessionState::Running {
        number: 2,
        since: SystemTime::now(),
    };
    let active = RunProgramView {
        program: "pi".to_owned(),
        label: "Pi".to_owned(),
        version: "1.4.0".to_owned(),
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:18889/v1".to_owned(),
        model: "same-model".to_owned(),
        codex_sandbox: None,
    };
    let next = RunProgramView {
        base_url: "http://127.0.0.1:18899/v1".to_owned(),
        ..active.clone()
    };
    one.active_program = Some(active);
    one.next_program = Some(next);
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    for (width, height) in [(72, 18), (120, 40)] {
        let frame = text(&mut board, width, height);
        assert!(
            frame.contains("127.0.0.1:18889") && frame.contains("127.0.0.1:18899"),
            "{width}x{height}: an address-only change is invisible: {frame}"
        );
    }
    press(&mut board, KeyCode::Char('p'));
    let frame = text(&mut board, 120, 40);
    assert!(
        frame.contains("127.0.0.1:18899") && frame.contains("next run: Pi"),
        "the Program preview lost the saved next address: {frame}"
    );
}

#[test]
fn c4_built_in_program_screen_names_the_shared_model_and_keeps_the_saved_one() {
    let mut one = agent(1);
    one.active_program = Some(RunProgramView {
        program: "dsh".to_owned(),
        label: "DeepSeek Harness".to_owned(),
        version: "0.2.1-rc.2".to_owned(),
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: "w73-dsh".to_owned(),
        codex_sandbox: None,
    });
    one.next_program = Some(RunProgramView {
        program: "builtin".to_owned(),
        label: "built-in engine".to_owned(),
        version: "always ready".to_owned(),
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: "w73-builtin".to_owned(),
        codex_sandbox: None,
    });
    let (mut board, _) = board(view(vec![one]));
    open_agent(&mut board);
    press(&mut board, KeyCode::Char('p'));
    let frame = text(&mut board, 120, 40);
    assert!(
        frame.contains("built-in uses shared model w73-builtin"),
        "{frame}"
    );
    assert!(
        frame.contains("saved DeepSeek Harness model w73-dsh is kept"),
        "{frame}"
    );
    assert!(
        frame.contains("next run: built-in engine always ready"),
        "{frame}"
    );
    assert!(!frame.contains("built-in engine not installed"), "{frame}");
}

#[test]
fn c4_the_agent_count_preview_speaks_in_the_future_tense() {
    let mut v = view(Vec::new());
    v.setup_complete = false;
    v.forgejo.repositories = 1;
    v.forgejo.repository_names = vec!["team/workspace".to_owned()];
    let (mut board, _) = board(v);
    assert_eq!(board.screen(), gewu_multiverse::board::Screen::Picker);
    let frame = text(&mut board, 80, 24);
    assert!(frame.contains("will create"), "{frame}");
    assert!(frame.contains("will run at"), "{frame}");
    assert!(!frame.contains("your login: owner"), "{frame}");
}

// ---- C4: the reopened board says what the wait is for ----

#[test]
fn c4_a_live_earlier_run_makes_the_reopen_say_what_it_is_stopping() {
    use gewu_multiverse::paths::Layout;
    let root = std::env::temp_dir().join(format!(
        "gewu-r36-startup-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch");
    let layout = Layout::rooted_at(root.join("config")).expect("layout");
    layout.ensure_root().expect("config root");
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data root");
    let run = layout.state_root().join("agent-1").join("run-1");
    std::fs::create_dir_all(&run).expect("run dir");
    std::fs::write(run.join("session.pid"), std::process::id().to_string()).expect("pid");
    std::fs::write(run.join("session.pgid"), std::process::id().to_string()).expect("pgid");
    assert!(
        gewu_multiverse::engine::stale_recovery_pending(&layout, &data),
        "a live earlier run was not seen before the board opened"
    );
    let claim = gewu_multiverse::engine::stale_recovery_claim(&layout, &data)
        .expect("the claim names what is being stopped");
    assert_eq!(claim.agent.as_deref(), Some("agent-1"));
    assert_eq!(claim.run_id.as_deref(), Some("run-1"));
    assert!(claim.message().contains("agent-1"), "{claim:?}");
    assert!(claim.message().contains("run-1"), "{claim:?}");
    let _ = std::fs::remove_file(run.join("session.pid"));
    let _ = std::fs::remove_file(run.join("session.pgid"));
    assert!(
        !gewu_multiverse::engine::stale_recovery_pending(&layout, &data),
        "a dead pid made the board promise a cleanup it will not do"
    );
    let _ = std::fs::remove_dir_all(&root);
}
