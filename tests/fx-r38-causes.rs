//! Wave 38 cause regressions: the 72x18 compact list, a run's own ending, the
//! warning word that matches the mark, the live pull-request state and the model
//! test's action at the two small sizes.
//!
//! Every case is rendered at a size named by the walk report; each one fails on
//! the base that reported the defect.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::{Board, BoardForm, Screen, status};
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, ProgramView, Provisioning, PullStateView, SessionEnd, SessionState,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
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

fn program(id: &str) -> ProgramView {
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
        installed_version: Some("1.0.0".to_owned()),
        installed: true,
        license: "MIT".to_owned(),
        download_mb: 10,
        prerequisite: None,
        prerequisite_met: true,
        installing: None,
        installing_phase: None,
        error: None,
    }
}

fn agent(number: usize) -> AgentView {
    AgentView {
        name: format!("agent-{number}"),
        login: format!("gewu-agent-{number}"),
        provisioning: Provisioning::Ready,
        desired: Desired::Stopped,
        session: SessionState::Idle,
        sessions_since_start: 0,
        runs_total: 4,
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
        program: "builtin".to_owned(),
        program_label: "built-in engine".to_owned(),
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
        programs: ["pi", "codex", "dsh", "opencode"]
            .into_iter()
            .map(program)
            .collect(),
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

fn mounted(agents: Vec<AgentView>) -> Board {
    Board::new(ViewEngine {
        view: view(agents),
        sent: Arc::new(Mutex::new(Vec::new())),
    })
}

fn press(board: &mut Board, code: KeyCode) {
    board.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn draw(board: &mut Board, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal.draw(|frame| board.draw(frame)).expect("draw");
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

fn open_agent(board: &mut Board) {
    press(board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);
}

fn open_model(board: &mut Board) {
    open_agent(board);
    press(board, KeyCode::Char('m'));
    assert_eq!(board.screen(), Screen::Model);
}

#[test]
fn the_72x18_board_is_one_wired_list_at_every_agent_count() {
    for count in [1, 3, 7] {
        let mut board = mounted((1..=count).map(agent).collect());
        let frame = text(&draw(&mut board, 72, 18));
        let layout = board.layout();
        assert_eq!(
            layout.form,
            BoardForm::List,
            "72x18 with {count} agents is not the compact list:\n{frame}"
        );
        assert_eq!(layout.listed.len(), count, "a line is missing:\n{frame}");
        assert_eq!(layout.cards.len(), 0, "a card is still drawn:\n{frame}");
        for line in &layout.listed {
            assert_eq!(line.rect.height, 1, "a line is not one row:\n{frame}");
        }
        assert!(
            !frame.contains('├') && !frame.contains('┤'),
            "72x18 with {count} agents draws a connector between rows:\n{frame}"
        );
    }
    // The larger pictures are unchanged.
    for (width, height) in [(80, 24), (120, 40)] {
        let mut board = mounted((1..=3).map(agent).collect());
        let _ = draw(&mut board, width, height);
        assert_eq!(
            board.layout().form,
            BoardForm::Cards,
            "{width}x{height} changed away from the card grid"
        );
    }
}

#[test]
fn a_stopped_run_shows_its_own_ending_and_labels_completed_work() {
    let mut stopped = agent(1);
    stopped.recent = vec![
        SessionEnd::Succeeded,
        SessionEnd::Stopped {
            reason: "the run was stopped on request".to_owned(),
            at: Some(SystemTime::now() - Duration::from_secs(180)),
        },
    ];
    stopped.last_work = vec![
        "issue #1".to_owned(),
        "pull request #4 from fix-readme-typo".to_owned(),
    ];
    stopped.last_result = Some("Pull request opened".to_owned());
    stopped.last_work_result = Some("Pull request opened".to_owned());
    stopped.last_result_is_work = false;
    let mut board = mounted(vec![stopped]);
    open_agent(&mut board);
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let frame = text(&draw(&mut board, width, height));
        assert!(
            frame.contains("stopped 3m ago"),
            "{width}x{height}: the stopped run's own age is missing:\n{frame}"
        );
        assert!(
            frame.contains("the run was stopped on request"),
            "{width}x{height}: the stopped run's own reason is missing:\n{frame}"
        );
        assert!(
            frame.contains("last completed work: PR #4 open"),
            "{width}x{height}: the completed work is not labelled as such:\n{frame}"
        );
        assert!(
            !frame.contains("result: Pull request opened"),
            "{width}x{height}: the stopped run wears the previous result:\n{frame}"
        );
    }
    // The board card pairs the earlier work with its own tick, never the newest
    // stopped mark.
    let mut board = mounted(vec![{
        let mut stopped = agent(1);
        stopped.recent = vec![
            SessionEnd::Succeeded,
            SessionEnd::Stopped {
                reason: "the run was stopped on request".to_owned(),
                at: Some(SystemTime::now()),
            },
        ];
        stopped.last_work = vec!["pull request #4 from fix-readme-typo".to_owned()];
        stopped.last_work_result = Some("Pull request opened".to_owned());
        stopped.last_result_is_work = false;
        stopped
    }]);
    let frame = text(&draw(&mut board, 120, 40));
    assert!(
        frame.contains("✓ #4"),
        "the card's work lost its own tick:\n{frame}"
    );
    assert!(
        !frame.contains("■ #4"),
        "the card pairs the stopped mark with earlier work:\n{frame}"
    );
}

#[test]
fn a_stopped_run_keeps_completed_work_before_a_failure_at_the_small_sizes() {
    let mut stopped = agent(1);
    stopped.recent = vec![
        SessionEnd::Succeeded,
        SessionEnd::Stopped {
            reason: "the run was stopped on request".to_owned(),
            at: Some(SystemTime::now() - Duration::from_secs(180)),
        },
    ];
    stopped.last_work = vec![
        "issue #1".to_owned(),
        "pull request team/workspace#4 from fix-readme-typo".to_owned(),
        "comment on team/workspace#4".to_owned(),
    ];
    stopped.last_failure = Some(
        "shell: exit 128: fatal: could not read from the remote repository because the \
         recorded detail is long enough to need another row"
            .to_owned(),
    );
    stopped.last_result = Some("the previous run's own summary".to_owned());
    stopped.last_work_result = Some("Pull request opened".to_owned());
    stopped.last_result_is_work = false;
    stopped.activity = Some("opened pull request #4".to_owned());
    stopped.activity_at = Some(SystemTime::now() - Duration::from_secs(180));
    let mut board = mounted(vec![stopped]);
    open_agent(&mut board);
    for (width, height) in [(72, 18), (80, 24)] {
        let frame = text(&draw(&mut board, width, height));
        let heading = frame
            .find("last completed work")
            .unwrap_or_else(|| panic!("{width}x{height}: no completed-work heading:\n{frame}"));
        let remote = frame
            .find("team/workspace#4")
            .unwrap_or_else(|| panic!("{width}x{height}: no remote work:\n{frame}"));
        let failure = frame
            .find("shell: exit 128")
            .unwrap_or_else(|| panic!("{width}x{height}: no failure detail:\n{frame}"));
        assert!(
            heading < remote && remote < failure,
            "{width}x{height}: work did not come before the failure:\n{frame}"
        );
        assert!(
            frame.contains("instruction"),
            "{width}x{height}: the instruction lost its heading:\n{frame}"
        );
    }
}

#[test]
fn a_successful_runs_result_stays_above_a_read_at_72x18() {
    let mut worked = agent(1);
    worked.recent = vec![SessionEnd::Succeeded];
    worked.last_work = vec![
        "issue #1".to_owned(),
        "pull request team/workspace#4 from fix-readme-typo".to_owned(),
    ];
    worked.last_result = Some("Pull request opened".to_owned());
    worked.last_work_result = Some("Pull request opened".to_owned());
    worked.last_result_is_work = true;
    worked.pull_states = vec![PullStateView {
        repository: "team/workspace".to_owned(),
        number: 4,
        state: "merged".to_owned(),
    }];
    let mut board = mounted(vec![worked]);
    open_agent(&mut board);
    let frame = text(&draw(&mut board, 72, 18));
    let result = frame
        .find("result: Pull request opened")
        .unwrap_or_else(|| panic!("72x18: the success result was pushed out:\n{frame}"));
    let read = frame
        .find("issue #1")
        .unwrap_or_else(|| panic!("72x18: the read row is missing:\n{frame}"));
    assert!(
        result < read,
        "72x18: a read outranked the run's own result:\n{frame}"
    );
    assert!(
        frame.contains("team/workspace#4 merged"),
        "72x18: the live request state was lost:\n{frame}"
    );
}

#[test]
fn the_dense_mixed_login_floor_row_keeps_a_push_repository() {
    let mut board_view = view((1..=4).map(agent).collect());
    board_view.agents[1].login = "alice".to_owned();
    board_view.agents[1].last_work = vec!["branch team/workspace#fix/readme".to_owned()];
    let mut board = Board::new(ViewEngine {
        view: board_view,
        sent: Arc::new(Mutex::new(Vec::new())),
    });
    let buffer = draw(&mut board, 72, 18);
    let slot = board
        .layout()
        .listed
        .iter()
        .find(|slot| slot.agent == 1)
        .expect("the mixed-login row")
        .rect;
    let mut row = String::new();
    for x in slot.x..slot.right() {
        row.push_str(buffer.cell((x, slot.y)).expect("a cell").symbol());
    }
    assert!(
        row.contains("team/workspace"),
        "the dense floor row lost the push repository: {row:?}"
    );
    assert!(
        row.contains("fix/read"),
        "the dense floor row lost the push branch: {row:?}"
    );
}

#[test]
fn a_stopped_run_with_one_pull_request_keeps_it_before_a_failure() {
    let mut stopped = agent(1);
    stopped.recent = vec![
        SessionEnd::Succeeded,
        SessionEnd::Stopped {
            reason: "the run was stopped on request".to_owned(),
            at: Some(SystemTime::now() - Duration::from_secs(180)),
        },
    ];
    stopped.last_work = vec![
        "issue #1".to_owned(),
        "pull request team/workspace#4 from fix-readme-typo".to_owned(),
    ];
    stopped.last_failure =
        Some("shell: exit 128: fatal: could not read from the remote repository".to_owned());
    stopped.last_result_is_work = false;
    let mut board = mounted(vec![stopped]);
    open_agent(&mut board);
    for (width, height) in [(72, 18), (80, 24)] {
        let frame = text(&draw(&mut board, width, height));
        let heading = frame
            .find("last completed work")
            .unwrap_or_else(|| panic!("{width}x{height}: no completed-work heading:\n{frame}"));
        let remote = frame
            .find("pull request team/workspace#4")
            .unwrap_or_else(|| panic!("{width}x{height}: no pull request row:\n{frame}"));
        let failure = frame
            .find("shell: exit 128")
            .unwrap_or_else(|| panic!("{width}x{height}: no failure detail:\n{frame}"));
        assert!(
            heading < remote && remote < failure,
            "{width}x{height}: the request did not come before the failure:\n{frame}"
        );
    }
}

#[test]
fn a_local_change_warning_is_named_in_the_strip_and_the_legend() {
    let mut local = agent(1);
    local.recent = vec![SessionEnd::Warned {
        reason: "workspace changed: workspace/TODO.md".to_owned(),
    }];
    local.last_work = vec!["local change workspace/TODO.md".to_owned()];
    local.last_result = Some("workspace changed: workspace/TODO.md".to_owned());
    local.last_work_result = Some("workspace changed: workspace/TODO.md".to_owned());
    local.last_result_is_work = true;
    assert_eq!(
        status::session_label(&local.recent[0]),
        "local change",
        "the newest warning label ignored the workspace reason"
    );
    let mut board = mounted(vec![local]);
    open_agent(&mut board);
    for (width, height) in [(72, 18), (80, 24)] {
        let frame = text(&draw(&mut board, width, height));
        assert!(
            frame.contains("local change"),
            "{width}x{height}: the newest warning is not named as a local change:\n{frame}"
        );
        assert!(
            frame.contains("⚠ warning"),
            "{width}x{height}: the legend does not describe the warning mark:\n{frame}"
        );
        assert!(
            !frame.contains("⚠ cmd"),
            "{width}x{height}: a local-only change still reads as a failed command:\n{frame}"
        );
    }
}

#[test]
fn a_merged_or_closed_request_is_not_shown_open() {
    for state in ["merged", "closed"] {
        let mut worked = agent(1);
        worked.recent = vec![SessionEnd::Succeeded];
        worked.last_work = vec![
            "issue #1".to_owned(),
            "pull request team/workspace#4 from fix-readme-typo".to_owned(),
        ];
        worked.last_result = Some("Pull request opened".to_owned());
        worked.last_work_result = Some("Pull request opened".to_owned());
        worked.last_result_is_work = true;
        worked.pull_states = vec![
            // Same number in another repository: it must not supply this state.
            PullStateView {
                repository: "team/alpha".to_owned(),
                number: 4,
                state: "open".to_owned(),
            },
            PullStateView {
                repository: "team/workspace".to_owned(),
                number: 4,
                state: state.to_owned(),
            },
        ];
        let mut board = mounted(vec![worked]);
        open_agent(&mut board);
        for (width, height) in [(72, 18), (80, 24), (120, 40)] {
            let frame = text(&draw(&mut board, width, height));
            assert!(
                frame.contains(&format!("team/workspace#4 {state}")),
                "{width}x{height}: the live {state} state is missing:\n{frame}"
            );
            assert!(
                !frame.contains("team/workspace#4 open"),
                "{width}x{height}: the run's recorded open state survived the {state}:\n{frame}"
            );
            assert!(
                !frame.contains("PR team/alpha#4"),
                "{width}x{height}: another repository's #4 supplied the state:\n{frame}"
            );
        }
        let home = text(&draw(&mut board, 120, 40));
        assert!(
            home.contains(&format!("team/workspace#4 {state}")),
            "the board card does not carry the live {state} state:\n{home}"
        );
    }
}

fn failed_test(reason: String) -> AgentView {
    let mut failed = agent(1);
    failed.program = "pi".to_owned();
    failed.program_label = "Pi".to_owned();
    failed.model_test = EndpointTest::Failed { reason };
    failed
}

#[test]
fn the_model_test_keeps_its_action_at_72x18_and_80x24() {
    let long = "Incorrect API key provided. You can find your API key at the provider dashboard. \
                The provider's own sentence continues for long enough that the form cannot show \
                all of it at once on the smallest screens.";
    for (reason, action) in [
        (
            format!("the provider refused the API key (HTTP 401): {long} — check the key"),
            "check the key",
        ),
        (
            format!(
                "the provider denied access to this model (HTTP 403): {long} — check the model and its access"
            ),
            "check the model and its access",
        ),
    ] {
        for (width, height) in [(72, 18), (80, 24)] {
            let mut board = mounted(vec![failed_test(reason.clone())]);
            open_model(&mut board);
            let frame = text(&draw(&mut board, width, height));
            assert!(
                frame.contains(action),
                "{width}x{height}: the model test cut its operative words:\n{frame}"
            );
        }
    }
}

fn failed_agent(reason: &str) -> AgentView {
    let mut failed = agent(1);
    failed.program = "dsh".to_owned();
    failed.program_label = "DeepSeek Harness".to_owned();
    failed.recent = vec![SessionEnd::Failed {
        reason: reason.to_owned(),
    }];
    failed.last_error = Some(reason.to_owned());
    failed.last_work.clear();
    failed.last_result = None;
    failed.last_work_result = None;
    failed.last_result_is_work = false;
    failed
}

#[test]
fn a_dsh_401_keeps_status_message_and_key_repair_at_every_size() {
    let raw = "AUTH: 401: {\"message\":\"Incorrect API key provided: sk-synthetic-provider-errors-0001\",\"type\":\"invalid_request_error\",\"code\":\"invalid_api_key\"}";
    let words = status::failure_words(raw);
    assert!(words.contains("HTTP 401"), "{words}");
    assert!(words.contains("Incorrect API key provided"), "{words}");
    assert!(words.contains("check the key"), "{words}");
    assert!(!words.contains('{'), "{words}");
    assert!(!words.contains("sk-synthetic"), "{words}");

    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut board = mounted(vec![failed_agent(raw)]);
        let home = text(&draw(&mut board, width, height));
        assert!(
            home.contains("401"),
            "{width}x{height}: the board lost the status:\n{home}"
        );
        assert!(
            home.contains("check key"),
            "{width}x{height}: the board lost the repair:\n{home}"
        );
        open_agent(&mut board);
        let frame = text(&draw(&mut board, width, height));
        assert!(
            frame.contains("HTTP 401"),
            "{width}x{height}: the Agent screen lost the status:\n{frame}"
        );
        assert!(
            frame.contains("check the key"),
            "{width}x{height}: the Agent screen lost the repair:\n{frame}"
        );
        assert!(
            !frame.contains("sk-synthetic"),
            "{width}x{height}: the credential echo survived:\n{frame}"
        );
        if width >= 80 {
            assert!(
                frame.contains("Incorrect API key provided"),
                "{width}x{height}: the provider's sentence is gone:\n{frame}"
            );
        }
    }
}

#[test]
fn a_dsh_403_keeps_access_guidance_and_never_says_check_the_key_at_every_size() {
    let raw = "AUTH: 403: {\"message\":\"You do not have permission to use this model.\",\"type\":\"permission_error\",\"code\":\"model_not_allowed\"}";
    let words = status::failure_words(raw);
    assert!(words.contains("HTTP 403"), "{words}");
    assert!(words.contains("access denied"), "{words}");
    assert!(words.contains("You do not have permission"), "{words}");
    assert!(words.contains("check the model and its access"), "{words}");
    assert!(!words.contains("check the key"), "{words}");

    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut board = mounted(vec![failed_agent(raw)]);
        let home = text(&draw(&mut board, width, height));
        assert!(
            home.contains("403"),
            "{width}x{height}: the board lost the status:\n{home}"
        );
        assert!(
            home.contains("check access"),
            "{width}x{height}: the board lost the access repair:\n{home}"
        );
        open_agent(&mut board);
        let frame = text(&draw(&mut board, width, height));
        assert!(
            frame.contains("HTTP 403"),
            "{width}x{height}: the Agent screen lost the status:\n{frame}"
        );
        assert!(
            frame.contains("check the model and its access"),
            "{width}x{height}: the Agent screen lost the access repair:\n{frame}"
        );
        assert!(
            !frame.contains("check the key"),
            "{width}x{height}: a 403 was still told as a key problem:\n{frame}"
        );
    }
}

#[test]
fn an_unknown_failure_keeps_its_words_and_an_ordinary_number_is_not_a_refusal() {
    let raw = "the native program exited with code 1";
    assert_eq!(status::failure_words(raw), raw);
    let words = status::failure_words("the model answered after 401 tokens and stopped");
    assert!(
        !words.contains("check the key") && !words.contains("HTTP 401"),
        "{words}"
    );
    let mut board = mounted(vec![failed_agent(raw)]);
    open_agent(&mut board);
    for (width, height) in [(80, 24), (120, 40)] {
        let frame = text(&draw(&mut board, width, height));
        assert!(
            frame.contains("exited with code 1"),
            "{width}x{height}: the useful unknown text is gone:\n{frame}"
        );
        assert!(
            !frame.contains("check the key") && !frame.contains("HTTP 401"),
            "{width}x{height}: invented provider advice:\n{frame}"
        );
    }
}

#[test]
fn the_board_keeps_the_provider_repair_with_three_agents() {
    let raw = "AUTH: 401: {\"message\":\"Incorrect API key provided: <redacted>\"}";
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut agents: Vec<AgentView> = (1..=3).map(agent).collect();
        agents[0] = failed_agent(raw);
        let mut board = mounted(agents);
        let home = text(&draw(&mut board, width, height));
        assert!(
            home.contains("check key"),
            "{width}x{height}: the board lost the repair with three agents:\n{home}"
        );
    }
}
