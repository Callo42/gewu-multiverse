//! Screen-truth regressions for the board, the model form and the first run.
//!
//! Every case is rendered at the size where it was reported broken. The board's wire is checked
//! against the card geometry, a promised key is pressed on the screen that promises
//! it, truncated sentences are read whole, and the product's own sentences are
//! checked against what the form and the engine do.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::{Board, Screen, status};
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, ProgramView, Provisioning, SessionEnd, SessionState, UpgradeAgent, UpgradeView,
};
use gewu_multiverse::model::{ModelError, failure_words};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

struct ViewEngine {
    view: BoardView,
    sent: Arc<Mutex<Vec<Command>>>,
}

impl EngineApi for ViewEngine {
    fn board(&mut self) -> BoardView {
        self.view.clone()
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
        if let Command::SetAgentModel { name, key, .. } = &command
            && key.as_deref() == Some("")
        {
            self.view.agents[0].key_present = false;
            self.view.notice = Some(format!(
                "{name}'s API key cleared; connection saved for its next run"
            ));
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
        version: "0.164.0".to_owned(),
        installed_version: Some("0.164.0".to_owned()),
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

fn programs() -> Vec<ProgramView> {
    ["pi", "codex", "dsh", "opencode"]
        .into_iter()
        .map(program)
        .collect()
}

fn agent(number: usize, program_id: &str) -> AgentView {
    AgentView {
        name: format!("agent-{number}"),
        login: format!("gewu-agent-{number}"),
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
            "Pi".to_owned()
        } else if program_id == "codex" {
            "Codex".to_owned()
        } else {
            "built-in engine".to_owned()
        },
        unknown_program: None,
        program_installed: true,
        program_version: Some("0.164.0".to_owned()),
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
        programs: programs(),
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

fn mounted(view: BoardView) -> (Board, Arc<Mutex<Vec<Command>>>) {
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

fn flat(buffer: &Buffer) -> String {
    text(buffer)
        .replace(['│', '╭', '╮', '╰', '╯', '─', '├', '┤', '┬', '┴', '┼'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn open_agent(board: &mut Board) {
    press(board, KeyCode::Enter);
    assert_eq!(board.screen(), Screen::Agent);
}

fn open_agent_model(board: &mut Board) {
    open_agent(board);
    press(board, KeyCode::Char('m'));
    assert_eq!(board.screen(), Screen::Model);
}

fn open_program(board: &mut Board) {
    open_agent(board);
    press(board, KeyCode::Char('p'));
    assert_eq!(board.screen(), Screen::Program);
}

/// The top-border junction of every card, in card order.
fn junctions(board: &Board, buffer: &Buffer) -> Vec<(usize, u16, Option<u16>)> {
    board
        .layout()
        .cards
        .iter()
        .map(|card| {
            let rect = card.rect;
            let found: Vec<u16> = (rect.x..rect.right())
                .filter(|x| {
                    let symbol = buffer.cell((*x, rect.y)).expect("top border").symbol();
                    symbol.chars().next().map(arms).unwrap_or_default() & U != 0
                })
                .collect();
            assert_eq!(
                found.len(),
                1,
                "card {} has {} top-border wire cells: {found:?}",
                card.agent,
                found.len()
            );
            (card.agent, rect.x, found.first().copied())
        })
        .collect()
}

#[test]
fn the_board_wire_is_fixed_geometry_and_never_crosses_a_card() {
    for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
        let (mut board, _sent) = mounted(view(vec![
            agent(1, "codex"),
            agent(2, "dsh"),
            agent(3, "pi"),
        ]));
        let first_frame = draw(&mut board, width, height);
        let first = junctions(&board, &first_frame);

        for card in &board.layout().cards {
            let rect = card.rect;
            let bottom = rect.bottom() - 1;
            for x in rect.x + 1..rect.right() - 1 {
                let symbol = first_frame
                    .cell((x, bottom))
                    .expect("bottom border")
                    .symbol();
                let cell_arms = symbol.chars().next().map(arms).unwrap_or_default();
                assert_eq!(
                    cell_arms & (U | D),
                    0,
                    "{width}x{height}: the connector enters card {}'s bottom border at ({x},{bottom}): {symbol:?}",
                    card.agent
                );
                let symbol = first_frame.cell((x, rect.y)).expect("top border").symbol();
                let cell_arms = symbol.chars().next().map(arms).unwrap_or_default();
                assert_eq!(
                    cell_arms & D,
                    0,
                    "{width}x{height}: a wire passes down through card {}'s top border at ({x},{}): {symbol:?}",
                    card.agent,
                    rect.y
                );
            }
            for y in rect.y + 1..bottom {
                for x in rect.x + 1..rect.right() - 1 {
                    let symbol = first_frame.cell((x, y)).expect("card body").symbol();
                    let cell_arms = symbol.chars().next().map(arms).unwrap_or_default();
                    assert_eq!(
                        cell_arms & (U | D),
                        0,
                        "{width}x{height}: a wire runs through card {}'s body at ({x},{y}): {symbol:?}",
                        card.agent
                    );
                }
            }
        }

        // Moving the selection cannot move any card or any drop.
        press(&mut board, KeyCode::Right);
        press(&mut board, KeyCode::Right);
        let second_frame = draw(&mut board, width, height);
        let second = junctions(&board, &second_frame);
        assert_eq!(
            first, second,
            "{width}x{height}: the wire moved when the selection moved"
        );
    }
}

#[test]
fn the_codex_warning_key_works_on_the_agent_screen() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut codex = agent(1, "codex");
        codex.program_error = Some(
            "Codex's sandbox cannot start (bwrap) — press f twice: removes Codex's own sandbox, keeps product Landlock"
                .to_owned(),
        );
        let (mut board, sent) = mounted(view(vec![codex]));
        open_agent(&mut board);

        let frame = flat(&draw(&mut board, width, height));
        assert!(frame.contains("press f twice"), "{frame}");
        assert!(frame.contains("f sandbox"), "{frame}");

        press(&mut board, KeyCode::Char('f'));
        assert!(
            sent.lock().expect("sent").is_empty(),
            "one f changed the sandbox without the second press"
        );
        assert!(
            board
                .notice()
                .is_some_and(|notice| notice.contains("Press f again")),
            "the first press did not say the second is needed: {:?}",
            board.notice()
        );
        press(&mut board, KeyCode::Char('f'));
        assert!(
            sent.lock().expect("sent").iter().any(|command| matches!(
                command,
                Command::SetAgentCodexSandbox { mode, .. } if mode == "full-access"
            )),
            "{width}x{height}: the Agent screen's press f twice did not reach the engine: {:?}",
            sent.lock().expect("sent")
        );
    }
}

#[test]
fn the_first_sandbox_press_names_the_second_on_the_rendered_agent_screen() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut codex = agent(1, "codex");
        codex.program_error = Some(
            "Codex's sandbox cannot start (bwrap) — press f twice: removes Codex's own sandbox, keeps product Landlock"
                .to_owned(),
        );
        let (mut board, _sent) = mounted(view(vec![codex]));
        open_agent(&mut board);
        press(&mut board, KeyCode::Char('f'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("Press f again"),
            "{width}x{height}: the rendered Agent notice lost the second press:\n{frame}"
        );
    }
}

#[test]
fn the_first_sandbox_press_names_the_second_on_the_rendered_program_screen() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let (mut board, _sent) = mounted(view(vec![agent(1, "codex")]));
        open_program(&mut board);
        press(&mut board, KeyCode::Char('f'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("Press f again"),
            "{width}x{height}: the rendered Program notice lost the second press:\n{frame}"
        );
    }
}

#[test]
fn the_rendered_key_clear_confirmation_says_it_was_cleared() {
    for (width, height) in [(72, 18), (80, 24)] {
        let (mut board, _sent) = mounted(view(vec![agent(1, "pi")]));
        open_agent_model(&mut board);
        press(&mut board, KeyCode::Down);
        press(&mut board, KeyCode::Down);
        press_ctrl(&mut board, 'u');
        press(&mut board, KeyCode::Enter);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("API key cleared"),
            "{width}x{height}: the rendered Agent confirmation lost the clear:\n{frame}"
        );
    }
}

#[test]
fn every_footer_state_keeps_a_two_row_notice_whole() {
    let notice =
        "agent-1's API key cleared; connection saved for its next run; this run keeps its settings";
    for (width, height) in [(72, 18), (80, 24)] {
        // Normal footer.
        let mut base = view(vec![agent(1, "pi")]);
        base.notice = Some(notice.to_owned());
        let mut normal = Board::new(ViewEngine {
            view: base.clone(),
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let frame = flat(&draw(&mut normal, width, height));
        assert!(
            frame.contains("settings"),
            "{width}x{height} normal footer:\n{frame}"
        );

        // Quit confirmation with a started agent.
        let mut running = agent(1, "pi");
        running.desired = Desired::Started;
        running.session = SessionState::Running {
            number: 1,
            since: SystemTime::now(),
        };
        let mut quit = view(vec![running]);
        quit.notice = Some(notice.to_owned());
        let mut quit = Board::new(ViewEngine {
            view: quit,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        press(&mut quit, KeyCode::Char('q'));
        let frame = flat(&draw(&mut quit, width, height));
        assert!(
            frame.contains("q again quits") && frame.contains("settings"),
            "{width}x{height} quit footer:\n{frame}"
        );

        // Remove confirmation on the board.
        let mut remove = view(vec![agent(1, "pi"), agent(2, "pi")]);
        remove.notice = Some(notice.to_owned());
        let mut remove = Board::new(ViewEngine {
            view: remove,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        press(&mut remove, KeyCode::Char('-'));
        let frame = flat(&draw(&mut remove, width, height));
        assert!(
            frame.contains("remove agent-1?") && frame.contains("settings"),
            "{width}x{height} board remove footer:\n{frame}"
        );

        // Remove confirmation on an Agent screen.
        let mut agent_remove = view(vec![agent(1, "pi"), agent(2, "pi")]);
        agent_remove.notice = Some(notice.to_owned());
        let mut agent_remove = Board::new(ViewEngine {
            view: agent_remove,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        open_agent(&mut agent_remove);
        press(&mut agent_remove, KeyCode::Char('d'));
        let frame = flat(&draw(&mut agent_remove, width, height));
        assert!(
            frame.contains("remove agent-1?") && frame.contains("settings"),
            "{width}x{height} agent remove footer:\n{frame}"
        );
    }
}

#[test]
fn a_long_notice_is_wrapped_whole_on_every_screen() {
    let notice =
        "a deliberately long product notice that must reach its final actionable word ACTION";
    for (width, height) in [(72, 18), (80, 24)] {
        let mut base = view(vec![agent(1, "pi")]);
        base.notice = Some(notice.to_owned());
        let mut board = Board::new(ViewEngine {
            view: base.clone(),
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let buffer = draw(&mut board, width, height);
        let raw = text(&buffer);
        let frame = flat(&buffer);
        assert!(
            frame.contains("ACTION"),
            "{width}x{height} home (footer {}):\n{raw}",
            board.layout().footer_rows
        );

        open_agent(&mut board);
        let frame = flat(&draw(&mut board, width, height));
        assert!(frame.contains("ACTION"), "{width}x{height} agent:\n{frame}");

        press(&mut board, KeyCode::Char('p'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("ACTION"),
            "{width}x{height} program:\n{frame}"
        );

        press(&mut board, KeyCode::Esc);
        press(&mut board, KeyCode::Char('m'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(frame.contains("ACTION"), "{width}x{height} model:\n{frame}");

        press(&mut board, KeyCode::Esc);
        press(&mut board, KeyCode::Esc);
        press(&mut board, KeyCode::Char('f'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("ACTION"),
            "{width}x{height} forgejo:\n{frame}"
        );

        press(&mut board, KeyCode::Esc);
        press(&mut board, KeyCode::Char('?'));
        let frame = flat(&draw(&mut board, width, height));
        assert!(frame.contains("ACTION"), "{width}x{height} help:\n{frame}");

        let mut first = view(Vec::new());
        first.terms_acknowledged = false;
        first.notice = Some(notice.to_owned());
        let mut terms = Board::new(ViewEngine {
            view: first,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let frame = flat(&draw(&mut terms, width, height));
        assert!(frame.contains("ACTION"), "{width}x{height} terms:\n{frame}");

        let mut picker = view(Vec::new());
        picker.notice = Some(notice.to_owned());
        let mut picker = Board::new(ViewEngine {
            view: picker,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let frame = flat(&draw(&mut picker, width, height));
        assert!(
            frame.contains("ACTION"),
            "{width}x{height} picker:\n{frame}"
        );

        let mut build = view(Vec::new());
        build.notice = Some(notice.to_owned());
        build.forgejo.state = ForgejoState::Failed {
            reason: "a deliberately long Forgejo start failure reason that keeps its end"
                .to_owned(),
        };
        let mut build = Board::new(ViewEngine {
            view: build,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        assert_eq!(build.screen(), Screen::Picker);
        press(&mut build, KeyCode::Enter);
        assert_eq!(build.screen(), Screen::Build);
        let frame = flat(&draw(&mut build, width, height));
        assert!(
            frame.contains("ACTION") && frame.contains("r retry"),
            "{width}x{height} build:\n{frame}"
        );

        let mut upgrade = view(Vec::new());
        upgrade.notice = Some(notice.to_owned());
        upgrade.upgrade = Some(UpgradeView {
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
            release_url: "https://github.com/Callo42/gewu-multiverse/releases/tag/v0.1.39"
                .to_owned(),
            whats_new: "run marks name what was observed".to_owned(),
            warnings_changed: false,
        });
        let mut upgrade = Board::new(ViewEngine {
            view: upgrade,
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        assert_eq!(upgrade.screen(), Screen::Upgrade);
        let frame = flat(&draw(&mut upgrade, width, height));
        assert!(
            frame.contains("ACTION"),
            "{width}x{height} upgrade:\n{frame}"
        );
    }
}

#[test]
fn the_dsh_install_key_works_on_the_agent_screen() {
    for (width, height) in [(72, 18), (80, 24)] {
        let mut dsh = agent(1, "dsh");
        dsh.program_error = Some(
            "DeepSeek Harness cannot start: install Node.js 24.9+ (npm 11.16+), then press i"
                .to_owned(),
        );
        let (mut board, sent) = mounted(view(vec![dsh]));
        open_agent(&mut board);
        let frame = flat(&draw(&mut board, width, height));
        assert!(frame.contains("then press i"), "{frame}");
        press(&mut board, KeyCode::Char('i'));
        assert!(
            sent.lock()
                .expect("sent")
                .iter()
                .any(|command| matches!(command, Command::InstallProgram { id } if id == "dsh")),
            "{width}x{height}: the Agent screen's press i did not install the program: {:?}",
            sent.lock().expect("sent")
        );
    }
}

#[test]
fn the_program_sandbox_sentence_is_whole_at_every_size() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let (mut board, _sent) = mounted(view(vec![agent(1, "codex")]));
        open_program(&mut board);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains(
                "Codex sandbox: its own default · press f twice: removes Codex's own sandbox, keeps product Landlock"
            ),
            "{width}x{height}: the sandbox sentence lost its action or consequence:\n{frame}"
        );

        let mut full = agent(1, "codex");
        full.codex_sandbox = Some("full-access".to_owned());
        let (mut board, _sent) = mounted(view(vec![full]));
        open_program(&mut board);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains(
                "Codex sandbox: sandbox off (product Landlock) · press f twice: returns Codex's own sandbox"
            ),
            "{width}x{height}: the restore sentence is cut:\n{frame}"
        );
    }
}

#[test]
fn the_wrong_protocol_notice_names_the_protocol_at_80_and_72() {
    for (width, height) in [(80, 24), (72, 18)] {
        let (mut board, _sent) = mounted(view(vec![agent(1, "pi")]));
        open_agent_model(&mut board);
        press(&mut board, KeyCode::Char('e'));
        press_ctrl(&mut board, 'u');
        type_text(&mut board, "https://model.example/v1/responses");
        press(&mut board, KeyCode::Enter);
        press(&mut board, KeyCode::Enter);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("change the protocol to OpenAI Responses"),
            "{width}x{height}: the notice no longer names the protocol to choose:\n{frame}"
        );
        assert!(
            !frame.contains("change the protocol to…"),
            "{width}x{height}: the notice was cut before its target:\n{frame}"
        );
    }
}

#[test]
fn the_board_keeps_the_branch_value_whole_or_ellipsized() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut one = agent(1, "codex");
        one.last_work = vec!["branch fix/readme-typo".to_owned()];
        let (mut board, _sent) = mounted(view(vec![one]));
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("fix/readme-typo"),
            "{width}x{height}: the card cut the branch name without the rest or an ellipsis:\n{frame}"
        );
    }

    // At 80x24 the card body carries `last work: …`; the branch value must survive
    // the line's own width, not stop after the label.
    let mut first = agent(1, "codex");
    first.last_work = vec!["branch agent-1/pr-only-1793168842".to_owned()];
    let mut second = agent(2, "pi");
    second.last_work = vec!["comment on #2".to_owned()];
    let mut third = agent(3, "dsh");
    third.last_work = vec!["comment on #3".to_owned()];
    let (mut board, _sent) = mounted(view(vec![first, second, third]));
    let frame = flat(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("last work: branch agent-1/"),
        "the card body stopped at the label instead of the branch:\n{frame}"
    );
}

#[test]
fn an_emptied_per_agent_key_is_saved_as_a_clear() {
    for (width, height) in [(80, 24), (72, 18)] {
        let (mut board, sent) = mounted(view(vec![agent(1, "pi")]));
        open_agent_model(&mut board);
        press(&mut board, KeyCode::Down);
        press(&mut board, KeyCode::Down);
        press_ctrl(&mut board, 'u');
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            !frame.contains("••••••••"),
            "{width}x{height}: ctrl-u left the old key's bullets on screen:\n{frame}"
        );
        press(&mut board, KeyCode::Enter);
        assert!(
            sent.lock().expect("sent").iter().any(|command| matches!(
                command,
                Command::SetAgentModel { key, .. } if key.as_deref() == Some("")
            )),
            "{width}x{height}: the emptied key was not saved as a clear: {:?}",
            sent.lock().expect("sent")
        );
    }
}

#[test]
fn a_403_is_an_access_problem_not_a_bad_key() {
    let words = failure_words(
        &ModelError::BadKey {
            status: 403,
            message: "You do not have access to model mock-1.".to_owned(),
        },
        "mock-1",
    );
    assert!(
        words.contains("You do not have access to model mock-1."),
        "{words}"
    );
    assert!(words.contains("access"), "{words}");
    assert!(!words.contains("refused the API key"), "{words}");
    assert!(!words.contains("check the key"), "{words}");

    // This is the real run path: the reason is built from the error's Display, so
    // the provider's sentence must already be in that text.
    let raw = format!(
        "the model call failed (bad_key): {} — model mock-1 at https://model.example/v1",
        ModelError::BadKey {
            status: 403,
            message: "You do not have access to model mock-1.".to_owned(),
        }
    );
    let board_words = status::failure_words(&raw);
    assert!(
        board_words.contains("You do not have access to model mock-1."),
        "{board_words}"
    );
    assert!(board_words.contains("access"), "{board_words}");
    assert!(!board_words.contains("check the key"), "{board_words}");

    let mut broken = agent(1, "pi");
    broken.session = SessionState::Idle;
    broken.recent = vec![SessionEnd::Failed {
        reason: raw.clone(),
    }];
    broken.last_error = Some(raw.clone());
    let (mut board, _sent) = mounted(view(vec![broken]));
    open_agent(&mut board);
    let frame = flat(&draw(&mut board, 80, 24));
    assert!(
        frame.contains("You do not have access to model mock-1."),
        "the provider's 403 sentence is gone:\n{frame}"
    );
    assert!(
        frame.contains("access denied") && frame.contains("check the model and its access"),
        "{frame}"
    );
    assert!(!frame.contains("check the key"), "{frame}");

    for (width, height) in [(80, 24), (72, 18)] {
        let mut model_broken = agent(1, "pi");
        model_broken.model_test = EndpointTest::Failed {
            reason: words.clone(),
        };
        let (mut board, _sent) = mounted(view(vec![model_broken]));
        open_agent_model(&mut board);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains("You do not have access to model mock-1."),
            "{width}x{height}: the model form lost the provider's 403 sentence:\n{frame}"
        );
        assert!(frame.contains("access"), "{frame}");
        assert!(!frame.contains("check the key"), "{frame}");
    }
}

#[test]
fn the_first_run_warning_matches_what_quitting_does() {
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut board = Board::new(gewu_multiverse::board::FakeEngine::new());
        assert_eq!(board.screen(), Screen::Terms);
        let frame = flat(&draw(&mut board, width, height));
        assert!(
            frame.contains(
                "Started agents keep running while the product is open; quitting stops them, and they start again on reopen."
            ),
            "{width}x{height}: the warning does not say what quitting does:\n{frame}"
        );
        assert!(
            !frame.contains("Started agents keep going until you stop them."),
            "{width}x{height}: the old open-ended promise is still on screen:\n{frame}"
        );
    }
}
