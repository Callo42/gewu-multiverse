//! fx-r35 layout matrix: the accepted cause/action/wait words, the last-work facts,
//! the three cleanup states and the quit warning at every supported size and agent
//! count. This is a deterministic UI guard; the live TUI evidence is separate.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::Board;
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, Provisioning, SessionEnd, SessionState,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

const SIZES: [(u16, u16); 5] = [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)];
const COUNTS: [usize; 4] = [1, 4, 7, 12];

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

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Working,
    Waiting,
    Stopped,
    Error,
    Uncertain,
    UnverifiedLeftover,
    LiveLeftover,
}

fn agent(number: usize, scenario: Scenario) -> AgentView {
    let login = format!("gewu-agent-{number}");
    let mut agent = AgentView {
        name: format!("agent-{number}"),
        login: login.clone(),
        provisioning: Provisioning::Ready,
        desired: Desired::Started,
        session: SessionState::Running {
            number: 1,
            since: SystemTime::now(),
        },
        sessions_since_start: 1,
        runs_total: number as u64 + 2,
        recent: vec![SessionEnd::Succeeded],
        instruction_chars: 20,
        instruction_first_line: Some("work the issue".to_owned()),
        instruction: Some("work the issue\n".to_owned()),
        instruction_lines: 1,
        activity: Some(format!("opened PR #{number}")),
        activity_at: Some(SystemTime::now() - Duration::from_secs(120)),
        last_error: None,
        last_result: Some(format!("I opened pull request #{number}")),
        last_failure: None,
        leftover: None,
        current_tool: None,
        current_tool_seconds: 0,
        last_work: vec![format!("pull request #{}", 20 + number)],
        no_assigned_work: false,
        last_work_result: Some(format!("I opened pull request #{number}")),
        last_result_is_work: true,
        program: "builtin".to_owned(),
        program_label: "built-in engine".to_owned(),
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
    };
    match scenario {
        Scenario::Working => {
            agent.current_tool = Some("shell".to_owned());
            agent.current_tool_seconds = 7;
        }
        Scenario::Waiting => {
            agent.session = SessionState::Restarting {
                next_at: SystemTime::now() + Duration::from_secs(252),
                last: SessionEnd::NoWork {
                    reason: "no open issue is assigned".to_owned(),
                },
            };
            agent.last_result = Some("no open issue is assigned".to_owned());
            agent.last_result_is_work = false;
            agent.no_assigned_work = true;
        }
        Scenario::Stopped => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Idle;
        }
        Scenario::Error => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Idle;
            agent.recent = vec![SessionEnd::Failed {
                reason: "the model call failed (server_error): the model answered HTTP 500"
                    .to_owned(),
            }];
            agent.last_result = None;
        }
        Scenario::Uncertain => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Blocked {
                reason: "cleanup unverified: the run started but its process records are lost; cleanup cannot be verified".to_owned(),
            };
            agent.leftover = Some(
                "cleanup unverified: the run started but its process records are lost; cleanup cannot be verified".to_owned(),
            );
        }
        Scenario::UnverifiedLeftover => {
            agent.desired = Desired::Stopped;
            let reason = "unverified leftover: a live process group (pgid 4242) cannot be verified as this run's; it was not signalled";
            agent.session = SessionState::Blocked {
                reason: reason.to_owned(),
            };
            agent.leftover = Some(reason.to_owned());
        }
        Scenario::LiveLeftover => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Blocked {
                reason: "live leftover: a run process (pid 4242) could not be stopped".to_owned(),
            };
            agent.leftover =
                Some("live leftover: a run process (pid 4242) could not be stopped".to_owned());
        }
    }
    agent
}

fn view(count: usize, scenario: Scenario) -> BoardView {
    let agents = (0..count).map(|index| agent(index + 1, scenario)).collect();
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

fn rect_text(buffer: &Buffer, rect: ratatui::layout::Rect) -> String {
    let mut out = String::new();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push(' ');
    }
    out
}

/// Every supported size and count keeps each agent's name, number, state mark and a
/// readable word for the five accepted states.
#[test]
fn every_state_keeps_its_words_at_every_size_and_count() {
    for (width, height) in SIZES {
        for count in COUNTS {
            let scenarios = [
                Scenario::Working,
                Scenario::Waiting,
                Scenario::Stopped,
                Scenario::Error,
                Scenario::Uncertain,
                Scenario::UnverifiedLeftover,
                Scenario::LiveLeftover,
            ];
            for scenario in scenarios {
                let mut board = Board::new(Fixed(view(count, scenario)));
                let buffer = draw(&mut board, width, height);
                let frame = text(&buffer);
                let label = format!("{width}x{height}/{count}/{scenario:?}");
                let layout = board.layout();
                assert_eq!(
                    layout.cards.len() + layout.listed.len(),
                    count,
                    "{label}: an agent slot was lost:\n{frame}"
                );
                for slot in layout.cards.iter().chain(layout.listed.iter()) {
                    let number = slot.agent + 1;
                    let slot_text = rect_text(&buffer, slot.rect);
                    assert!(
                        slot_text.contains(&format!("agent-{number}")),
                        "{label}: agent-{number} was lost:\n{slot_text}"
                    );
                    let runs = view(count, scenario).agents[slot.agent].runs_total;
                    assert!(
                        slot_text.contains(&format!("run {runs}"))
                            || slot_text.contains(&format!("run{runs}"))
                            || slot_text
                                .split(|ch: char| !ch.is_ascii_digit())
                                .any(|digits| digits == runs.to_string()),
                        "{label}: agent-{number}'s run count was lost:\n{slot_text}"
                    );
                    let login = &view(count, scenario).agents[slot.agent].login;
                    if slot_text.contains(login.as_str()) {
                        // The whole login is on the line.
                    } else {
                        // The box legend names the exact generated-login rule.
                        assert_eq!(
                            *login,
                            format!("gewu-agent-{number}"),
                            "{label}: agent-{number}'s non-standard login was dropped:\n{slot_text}"
                        );
                        assert!(
                            frame.contains("login=gewu-<name>"),
                            "{label}: the login rule is not in the list legend:\n{frame}"
                        );
                    }
                    assert!(
                        slot_text.contains('○')
                            || slot_text.contains('◐')
                            || slot_text.contains('●'),
                        "{label}: agent-{number}'s state mark was lost:\n{slot_text}"
                    );
                }
                match scenario {
                    Scenario::Working => {
                        assert!(
                            frame.contains("working") || frame.contains("shell · 7s"),
                            "{label}: no working word:\n{frame}"
                        );
                    }
                    Scenario::Waiting => {
                        assert!(
                            frame.contains("no work"),
                            "{label}: the no-work wait word was lost:\n{frame}"
                        );
                        assert!(
                            !frame.contains("no w…"),
                            "{label}: the no-work wait word was cut:\n{frame}"
                        );
                    }
                    Scenario::Stopped => {
                        assert!(
                            frame.contains('○') || frame.contains("stopped"),
                            "{label}: no stopped state:\n{frame}"
                        );
                    }
                    Scenario::Error => {
                        assert!(
                            frame.contains("HTTP 500") || frame.contains("✗ 500"),
                            "{label}: the failure's HTTP status was lost:\n{frame}"
                        );
                    }
                    Scenario::Uncertain => {
                        assert!(
                            frame.contains("cleanup unverified"),
                            "{label}: the uncertain state lost its name:\n{frame}"
                        );
                        assert!(
                            !frame.contains("leftover process"),
                            "{label}: uncertainty was called a live leftover:\n{frame}"
                        );
                        assert!(
                            !frame.contains("cleanup unver…"),
                            "{label}: the uncertain state was cut:\n{frame}"
                        );
                        // Every "cannot be verified" reason is still uncertainty;
                        // the exact prefix, not the shared phrase, picks the state.
                        assert!(
                            !frame.contains("unverified leftover"),
                            "{label}: record uncertainty was relabelled as a live leftover:\n{frame}"
                        );
                    }
                    Scenario::UnverifiedLeftover => {
                        assert!(
                            frame.contains("unverified leftover") || frame.contains("⚠ unowned"),
                            "{label}: a live-but-unowned process lost its name:\n{frame}"
                        );
                        assert!(
                            !frame.contains("cleanup unverified"),
                            "{label}: live-but-unowned was called record uncertainty:\n{frame}"
                        );
                    }
                    Scenario::LiveLeftover => {
                        assert!(
                            frame.contains("live leftover"),
                            "{label}: a proven live leftover lost its name:\n{frame}"
                        );
                        assert!(
                            !frame.contains("live left…"),
                            "{label}: the live leftover word was cut:\n{frame}"
                        );
                    }
                }
            }
        }
    }
}

/// The roomy cards carry the documented newest activity and time, and the twelve
/// agent grid keeps the short last-work word on the border.
/// The exact login is either inline in this row or carried by a row token whose
/// full `@token=login` map is on the frame.
fn row_has_agent_name(frame: &str, row: &str, agent: &AgentView, _index: usize) -> bool {
    row.contains(&agent.name)
        || agent.name.strip_prefix("agent-").is_some_and(|number| {
            row.contains(&format!("{number}:")) && frame.contains("N=agent-N")
        })
}

fn row_has_exact_login(frame: &str, row: &str, login: &str) -> bool {
    if row.contains(login) {
        return true;
    }
    row.split_whitespace().any(|word| {
        word.split_once('@').is_some_and(|(_, token)| {
            frame
                .lines()
                .any(|line| line.contains(&format!("@{token}={login}")))
        })
    })
}

#[test]
fn roomy_cards_show_last_activity_and_time_and_the_twelve_grid_keeps_last_work() {
    for (width, height) in [(100, 30), (120, 40), (200, 60)] {
        for count in [1, 4, 7, 12] {
            let mut board = Board::new(Fixed(view(count, Scenario::Stopped)));
            let buffer = draw(&mut board, width, height);
            let frame = text(&buffer);
            let layout = board.layout();
            let cards = &layout.cards;
            if cards.is_empty() {
                continue;
            }
            for slot in cards {
                let number = slot.agent + 1;
                let body = rect_text(&buffer, slot.rect);
                if slot.rect.height >= 5 {
                    assert!(
                        body.contains("last:") && body.contains("opened") && body.contains('m'),
                        "{width}x{height}/{count}: agent-{number} has no activity/time:\n{body}"
                    );
                }
                assert!(
                    body.contains(&format!("#{}", 20 + number)),
                    "{width}x{height}/{count}: agent-{number} lost the short last-work word:\n{body}"
                );
            }
            assert!(
                frame.contains("run N") || frame.contains("run "),
                "{width}x{height}/{count}: the run legend/count was lost"
            );
        }
    }
}

/// The Agent screen keeps the cleanup state and the model's own cause whole at the
/// two floor sizes.
#[test]
fn the_cleanup_cause_and_model_summary_are_whole_at_the_floor() {
    for (width, height) in [(72, 18), (80, 24)] {
        for scenario in [
            Scenario::Uncertain,
            Scenario::UnverifiedLeftover,
            Scenario::LiveLeftover,
        ] {
            let mut board = Board::new(Fixed(view(1, scenario)));
            board.on_key(KeyEvent::from(KeyCode::Enter));
            let frame = text(&draw(&mut board, width, height));
            let wanted = match scenario {
                Scenario::Uncertain => "cleanup unverified",
                Scenario::UnverifiedLeftover => "unverified leftover",
                Scenario::LiveLeftover => "live leftover",
                _ => unreachable!(),
            };
            assert!(
                frame.contains(wanted),
                "{width}x{height}: {wanted} was cut:\n{frame}"
            );
            assert!(
                frame.contains("last work:") || frame.contains("result:"),
                "{width}x{height}: the last work/result row was lost:\n{frame}"
            );
            // The model's own sentence is kept, not translated into a status label.
            assert!(
                frame.contains("pull request #21")
                    || frame.contains("last work:")
                    || frame.contains("result:"),
                "{width}x{height}: the work identity was lost:\n{frame}"
            );
        }
    }
}

/// The quit warning, the repository event and the footer facts survive the same
/// matrix, with and without each optional row.
#[test]
fn the_quit_warning_and_event_rows_survive_the_matrix() {
    for (width, height) in SIZES {
        for count in COUNTS {
            for event in [false, true] {
                let mut many = view(count, Scenario::Working);
                if !event {
                    many.forgejo.events.clear();
                }
                let mut board = Board::new(Fixed(many));
                let normal = text(&draw(&mut board, width, height));
                assert!(
                    normal.contains("s start/stop") || normal.contains("s stop"),
                    "{width}x{height}/{count}: the key footer was lost:\n{normal}"
                );
                if event {
                    assert!(
                        normal.contains("team/scratch was deleted"),
                        "{width}x{height}/{count}: the repository event was lost:\n{normal}"
                    );
                }
                board.on_key(KeyEvent::from(KeyCode::Char('q')));
                let frame = text(&draw(&mut board, width, height));
                let flat = frame
                    .replace(['│', '─', '╭', '╮', '╰', '╯'], " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    flat.contains("quit stops"),
                    "{width}x{height}/{count}: no quit warning:\n{frame}"
                );
                assert!(
                    flat.contains("q again quits"),
                    "{width}x{height}/{count}: the reopen fact was lost:\n{frame}"
                );
            }
        }
    }
}

/// Each cleanup field — state word, cause row, card/list word — is checked for the
/// exact category, including an ending-read error that contains "cannot be verified"
/// but is not a live process.
#[test]
fn every_cleanup_field_keeps_its_exact_category() {
    let cases = [
        (
            "cleanup unverified: the run's ending cannot be read, so cleanup cannot be verified: permission denied",
            "cleanup unverified",
            "⚠ cleanup unverified",
            "⚠ unverified",
        ),
        (
            "unverified leftover: a live process group (pgid 4242) cannot be verified as this run's",
            "unverified leftover",
            "⚠ unverified leftover",
            "⚠ unowned",
        ),
        (
            "live leftover: a run process (pid 4242) could not be stopped",
            "live leftover",
            "⚠ live leftover",
            "⚠ live",
        ),
    ];
    for (reason, blocked, full, short) in cases {
        let mut board_view = view(1, Scenario::Stopped);
        board_view.agents[0].session = SessionState::Blocked {
            reason: reason.to_owned(),
        };
        board_view.agents[0].leftover = Some(reason.to_owned());
        for (width, height, want) in [(72, 18, full), (80, 24, full), (120, 40, full)] {
            let mut board = Board::new(Fixed(board_view.clone()));
            let home_buffer = draw(&mut board, width, height);
            let home = text(&home_buffer);
            let rect = {
                let layout = board.layout();
                layout
                    .cards
                    .first()
                    .or_else(|| layout.listed.first())
                    .expect("a card")
                    .rect
            };
            let field = rect_text(&home_buffer, rect);
            assert!(
                field.contains(want),
                "{width}x{height}: the card/list word is not the exact category {want:?}: {field}"
            );
            assert!(
                home.contains(blocked),
                "{width}x{height}: the state word {blocked:?} is missing:\n{home}"
            );
            // The Agent screen state and cause row carry the same category.
            let mut agent_board = Board::new(Fixed(board_view.clone()));
            agent_board.on_key(KeyEvent::from(KeyCode::Enter));
            let agent_frame = text(&draw(&mut agent_board, width, height));
            assert!(
                agent_frame.contains(blocked),
                "{width}x{height}: the Agent state lost {blocked:?}:\n{agent_frame}"
            );
            assert!(
                agent_frame.contains(full),
                "{width}x{height}: the Agent cause row lost {full:?}:\n{agent_frame}"
            );
        }
        assert_eq!(
            gewu_multiverse::board::status::leftover_short(reason),
            short,
            "the compact word is not the exact category"
        );
    }
}

/// A roomy card's newest-activity row must keep a truthful age on that same card.
/// The 200x60 twelve-agent frame exposed the non-detailed branch emitting the raw
/// full line and cutting the age off; the 120x40 grid exposed the compact branch
/// ellipsizing the whole string.
#[test]
fn roomy_card_activity_keeps_its_age_on_the_agents_own_card() {
    for (width, height) in [(120, 40), (200, 60)] {
        let mut board_view = view(12, Scenario::Stopped);
        let target = 7;
        board_view.agents[target].activity = Some("opened pull request #20".to_owned());
        board_view.agents[target].activity_at = Some(SystemTime::now() - Duration::from_secs(7200));
        board_view.agents[target].last_result = None;
        board_view.agents[target].last_work_result = None;
        board_view.agents[target].last_work.clear();
        board_view.agents[target].last_result_is_work = false;
        let mut board = Board::new(Fixed(board_view));
        let buffer = draw(&mut board, width, height);
        let rect = {
            let layout = board.layout();
            layout
                .cards
                .iter()
                .find(|slot| slot.agent == target)
                .unwrap_or_else(|| panic!("{width}x{height}: agent {target} has no card"))
                .rect
        };
        let field = rect_text(&buffer, rect);
        let activity = field
            .lines()
            .find(|line| line.contains("last:"))
            .unwrap_or_else(|| {
                panic!("{width}x{height}: no activity row on agent's card:\n{field}")
            });
        assert!(
            activity.contains("2h"),
            "{width}x{height}: the age was cut from the card row: {activity:?}\n{field}"
        );
        assert!(
            activity.contains("#20"),
            "{width}x{height}: the action identity was cut: {activity:?}\n{field}"
        );
    }
}

/// Every cleanup card at the wide twelve-agent grid keeps a distinct whole word,
/// even when six narrow cards share the 160-column picture.
#[test]
fn wide_twelve_grid_cleanup_labels_are_whole_and_distinct() {
    for scenario in [
        Scenario::Uncertain,
        Scenario::UnverifiedLeftover,
        Scenario::LiveLeftover,
    ] {
        for (width, height) in [(120, 40), (200, 60)] {
            let mut board = Board::new(Fixed(view(12, scenario)));
            let buffer = draw(&mut board, width, height);
            let cards = board.layout().cards.clone();
            assert!(!cards.is_empty(), "{width}x{height}: no cards");
            for slot in cards {
                let field = rect_text(&buffer, slot.rect);
                assert!(
                    !field.contains("cleanup…")
                        && !field.contains("unverified…")
                        && !field.contains("live…"),
                    "{width}x{height}: agent {}'s cleanup label was cut:\n{field}",
                    slot.agent
                );
                assert!(
                    field.contains("⚠ unverified")
                        || field.contains("cleanup unverified")
                        || field.contains("⚠ unowned")
                        || field.contains("unverified leftover")
                        || field.contains("⚠ live")
                        || field.contains("live leftover"),
                    "{width}x{height}: agent {} has no distinct cleanup word:\n{field}",
                    slot.agent
                );
            }
        }
    }
}

/// At 80x24 with one or four agents the card is five rows but not "detailed".
/// A normal finished run has both a summary and an activity; the activity row must
/// still carry the newest action/number and its age on that agent's own card.
#[test]
fn the_80x24_activity_row_outranks_a_normal_summary() {
    for count in [1, 4] {
        let mut board_view = view(count, Scenario::Stopped);
        let target = 0;
        board_view.agents[target].activity = Some("opened pull request #20".to_owned());
        board_view.agents[target].activity_at = Some(SystemTime::now() - Duration::from_secs(7200));
        // Leave the normal summary and last-work result populated; this is the
        // ordinary finished-run shape, not a cleared-field fixture.
        assert!(board_view.agents[target].last_result.is_some());
        assert!(board_view.agents[target].last_work_result.is_some());
        let summary = board_view.agents[target]
            .last_result
            .clone()
            .expect("the summary is kept");
        let mut board = Board::new(Fixed(board_view.clone()));
        let buffer = draw(&mut board, 80, 24);
        let rect = {
            let layout = board.layout();
            layout
                .cards
                .iter()
                .find(|slot| slot.agent == target)
                .unwrap_or_else(|| panic!("{count} agents: agent {target} has no card"))
                .rect
        };
        let field = rect_text(&buffer, rect);
        let activity = field
            .lines()
            .find(|line| line.contains("last:"))
            .unwrap_or_else(|| panic!("{count} agents: no last activity on own card:\n{field}"));
        assert!(
            activity.contains("2h") && activity.contains("#20"),
            "{count} agents: the activity action/age was suppressed: {activity:?}\n{field}"
        );
        // The model's sentence is still in the existing Agent detail view, not lost
        // and not substituted for the activity row.
        let mut detail = Board::new(Fixed(board_view));
        detail.on_key(KeyEvent::from(KeyCode::Enter));
        let frame = text(&draw(&mut detail, 120, 40));
        assert!(
            frame.contains(&summary),
            "{count} agents: the model summary left the Agent screen: {summary:?}\n{frame}"
        );
    }
}

/// A persisted non-standard login must stay exact at the two-column 72x18 floor,
/// alongside its name, state, run count and last-work word.
#[test]
fn the_floor_keeps_a_persisted_nonstandard_login() {
    for count in [4, 7, 12] {
        for scenario in [Scenario::Waiting, Scenario::Error, Scenario::Uncertain] {
            let mut board_view = view(count, scenario);
            let target = 1;
            board_view.agents[target].login = "alice".to_owned();
            let target_agent = board_view.agents[target].clone();
            let count_text = board_view.agents[target].runs_total.to_string();
            let mut board = Board::new(Fixed(board_view));
            let buffer = draw(&mut board, 72, 18);
            let rect = {
                let layout = board.layout();
                layout
                    .listed
                    .iter()
                    .find(|slot| slot.agent == target)
                    .unwrap_or_else(|| panic!("{count}: agent {target} has no list row"))
                    .rect
            };
            let row = rect_text(&buffer, rect);
            let frame = text(&buffer);
            assert!(
                row_has_exact_login(&frame, &row, "alice"),
                "{count}/{scenario:?}: the recorded login was dropped from row/legend: {row:?}\n{frame}"
            );
            assert!(
                row_has_agent_name(&frame, &row, &target_agent, target)
                    && row.contains(&count_text),
                "{count}/{scenario:?}: name/run count lost beside the login: {row:?}"
            );
            match scenario {
                Scenario::Waiting => assert!(
                    row.contains('◐') && frame.contains("waiting"),
                    "{count}: the waiting mark/legend is absent: {row:?}\n{frame}"
                ),
                Scenario::Error => assert!(
                    row.contains('✗') || frame.contains("stopped"),
                    "{count}: the error state is absent: {row:?}\n{frame}"
                ),
                Scenario::Uncertain => assert!(
                    row.trim_end().ends_with('u') && frame.contains("u=unverified"),
                    "{count}: the uncertain code/legend is absent: {row:?}\n{frame}"
                ),
                _ => unreachable!(),
            }
            assert!(
                row.contains('#'),
                "{count}/{scenario:?}: last-work identity lost: {row:?}"
            );
            if matches!(scenario, Scenario::Error) {
                assert!(
                    row.contains('✗'),
                    "{count}/{scenario:?}: the failure cause was lost: {row:?}"
                );
            }
            assert!(
                !frame.contains("login=gewu-<name>"),
                "{count}/{scenario:?}: a generated-login legend claims a non-standard login"
            );
        }
    }
}

/// The same supported identity path with a valid longer recorded login. The compact
/// paragraph must keep the exact login, name, state/category, count, last-work and
/// legend without imposing a login-length cap or cutting any of them.
#[test]
fn the_floor_keeps_a_long_persisted_login() {
    for count in [4, 7, 12] {
        for scenario in [Scenario::Waiting, Scenario::Error, Scenario::Uncertain] {
            let mut board_view = view(count, scenario);
            let target = 1;
            let login = "documentation-bot".to_owned();
            board_view.agents[target].login = login.clone();
            let target_agent = board_view.agents[target].clone();
            let count_text = board_view.agents[target].runs_total.to_string();
            let mut board = Board::new(Fixed(board_view));
            let buffer = draw(&mut board, 72, 18);
            let rect = {
                let layout = board.layout();
                layout
                    .listed
                    .iter()
                    .find(|slot| slot.agent == target)
                    .unwrap_or_else(|| panic!("{count}: agent {target} has no row"))
                    .rect
            };
            let row = rect_text(&buffer, rect);
            let frame = text(&buffer);
            assert!(
                row_has_exact_login(&frame, &row, &login),
                "{count}/{scenario:?}: exact longer login missing from row/legend: {row:?}\n{frame}"
            );
            assert!(
                !row.contains("documentation-…") && !row.contains("documentation…"),
                "{count}/{scenario:?}: the login was partially cut: {row:?}"
            );
            assert!(
                row_has_agent_name(&frame, &row, &target_agent, target)
                    && row.contains(&count_text)
                    && row.contains('#'),
                "{count}/{scenario:?}: name/count/last-work lost: {row:?}"
            );
            match scenario {
                Scenario::Waiting => assert!(
                    row.contains('◐') && (frame.contains("wait") || frame.contains("next")),
                    "{count}: waiting state/legend missing: {row:?}\n{frame}"
                ),
                Scenario::Error => assert!(
                    row.contains('✗') && (frame.contains("stop") || frame.contains("failed")),
                    "{count}: error state/legend missing: {row:?}\n{frame}"
                ),
                Scenario::Uncertain => assert!(
                    row.trim_end().ends_with('u') && frame.contains("u=unverified"),
                    "{count}: uncertain category/legend missing: {row:?}\n{frame}"
                ),
                _ => unreachable!(),
            }
            assert!(
                !frame.contains("login=gewu-<name>"),
                "{count}/{scenario:?}: a generated-login legend claims a persisted login"
            );
        }
    }
}

/// A mixed board must keep every shown identity exact and associated with its agent:
/// generated logins by their inline value or a marked generated rule, and two
/// distinct longer recorded logins by an agent-mapped token. Every row keeps its
/// name, state mark, count and last-work/cleanup facts.
#[test]
fn mixed_boards_keep_every_identity_and_agent_mapping() {
    for count in [4, 7, 12] {
        for scenario in [Scenario::Waiting, Scenario::Error, Scenario::Uncertain] {
            let mut board_view = view(count, scenario);
            board_view.agents[1].login = "documentation-bot".to_owned();
            if count >= 7 {
                board_view.agents[5].login = "second-recorded-identity".to_owned();
            }
            let mut board = Board::new(Fixed(board_view.clone()));
            let buffer = draw(&mut board, 72, 18);
            let frame = text(&buffer);
            let layout = board.layout();
            assert!(
                !layout.listed.is_empty(),
                "{count}/{scenario:?}: no list rows"
            );
            for slot in &layout.listed {
                let agent = &board_view.agents[slot.agent];
                let row = rect_text(&buffer, slot.rect);
                assert!(
                    row_has_agent_name(&frame, &row, agent, slot.agent),
                    "{count}/{scenario:?}: {} lost its name: {row:?}",
                    agent.name
                );
                assert!(
                    row.contains(&agent.runs_total.to_string()),
                    "{count}/{scenario:?}: {} lost its run count: {row:?}",
                    agent.name
                );
                assert!(
                    row.contains('#'),
                    "{count}/{scenario:?}: {} lost its last-work identity: {row:?}",
                    agent.name
                );
                let generated = format!("gewu-{}", agent.name);
                if agent.login == generated {
                    let represented = row.contains(&agent.login)
                        || (row.contains('*') && frame.contains("*=gewu-<name>"));
                    assert!(
                        represented,
                        "{count}/{scenario:?}: generated identity {} is lost: {row:?}\n{frame}",
                        agent.login
                    );
                } else {
                    let mapped = frame.lines().any(|line| {
                        line.contains(agent.login.as_str())
                            && row.split_whitespace().any(|word| {
                                word.split_once('@').is_some_and(|(_, token)| {
                                    line.contains(&format!("@{token}={}", agent.login))
                                })
                            })
                    });
                    assert!(
                        row.contains(&agent.login) || mapped,
                        "{count}/{scenario:?}: recorded identity {} has no row/token mapping: {row:?}\n{frame}",
                        agent.login
                    );
                }
                assert!(
                    row.contains('◐') || row.contains('○') || row.contains('●'),
                    "{count}/{scenario:?}: {} lost its state mark: {row:?}",
                    agent.name
                );
            }
            assert!(
                !frame.contains("login documentation-bot · s…")
                    && !frame.contains("login documentation-bot · second"),
                "{count}/{scenario:?}: persisted identities are one clipped string:\n{frame}"
            );
        }
    }
}

/// Every supported visible identity count is recovered from the frame. The dense
/// name rule is only used for the actual `agent-N` equality; each recorded login is
/// still printed whole and associated with its own row.
#[test]
fn dense_identity_mapping_handles_every_supported_identity_count() {
    let logins = [
        "documentation-bot",
        "second-recorded-identity",
        "third-recorded-login",
        "fourth-recorded-login",
        "fifth-recorded-login",
        "sixth-recorded-login",
        "seventh-recorded-login",
        "eighth-recorded-login",
        "ninth-recorded-login",
        "tenth-recorded-login",
        "eleventh-recorded-login",
        "twelfth-recorded-login",
    ];

    for count in [4, 7, 12] {
        for identities in [1, 2, 3, 4, 7, 12] {
            if identities > count {
                continue;
            }
            for scenario in [Scenario::Waiting, Scenario::Error, Scenario::Uncertain] {
                let mut board_view = view(count, scenario);
                for (index, login) in logins.iter().take(identities).enumerate() {
                    board_view.agents[index].login = (*login).to_owned();
                }
                let mut board = Board::new(Fixed(board_view.clone()));
                let buffer = draw(&mut board, 72, 18);
                let frame = text(&buffer);
                let layout = board.layout();
                assert_eq!(layout.listed.len(), count, "{count} agents not all visible");
                for slot in &layout.listed {
                    let agent = &board_view.agents[slot.agent];
                    let row = rect_text(&buffer, slot.rect);
                    assert!(
                        row_has_agent_name(&frame, &row, agent, slot.agent),
                        "{count}/{identities}/{scenario:?}: name lost: {row:?}\n{frame}"
                    );
                    assert!(
                        row.contains(&agent.login),
                        "{count}/{identities}/{scenario:?}: exact login lost: {row:?}\n{frame}"
                    );
                    assert!(
                        row.contains(&agent.runs_total.to_string()) && row.contains('#'),
                        "{count}/{identities}/{scenario:?}: count/last-work lost: {row:?}"
                    );
                    assert!(
                        row.contains('◐') || row.contains('○') || row.contains('●'),
                        "{count}/{identities}/{scenario:?}: state mark lost: {row:?}"
                    );
                    if matches!(scenario, Scenario::Error) {
                        assert!(
                            row.contains('✗'),
                            "{count}/{identities}: failure lost: {row:?}"
                        );
                    }
                    if matches!(scenario, Scenario::Uncertain) {
                        assert!(
                            row.trim_end().ends_with('u') && frame.contains("u=unverified"),
                            "{count}/{identities}: category/legend lost: {row:?}\n{frame}"
                        );
                    }
                }
            }
        }
    }
}

/// Removing an agent leaves its surviving neighbours' saved names unchanged.
#[test]
fn dense_rows_keep_gapped_agent_numbers_and_recorded_login_facts() {
    for count in [4, 7, 12] {
        for scenario in [Scenario::Waiting, Scenario::Error, Scenario::Uncertain] {
            let mut board_view = view(count, scenario);
            for (index, agent) in board_view.agents.iter_mut().enumerate() {
                let number = if index == 0 { 1 } else { index + 2 };
                agent.name = format!("agent-{number}");
                agent.login = format!("gewu-agent-{number}");
            }
            board_view.agents[0].login = "documentation-bot".to_owned();
            board_view.agents[count - 1].login = "second-recorded-identity".to_owned();
            let mut board = Board::new(Fixed(board_view.clone()));
            let buffer = draw(&mut board, 72, 18);
            let frame = text(&buffer);
            let layout = board.layout();
            assert_eq!(layout.listed.len(), count);
            for slot in &layout.listed {
                let agent = &board_view.agents[slot.agent];
                let row = rect_text(&buffer, slot.rect);
                assert!(
                    row_has_agent_name(&frame, &row, agent, slot.agent),
                    "name lost: {row:?}"
                );
                assert!(row.contains(&agent.login), "login lost: {row:?}");
                let last_work = format!("#{}", 21 + slot.agent);
                let facts = format!("{}{last_work}", agent.runs_total);
                assert!(row.contains(&facts), "count/last-work lost: {row:?}");
                let mark = if matches!(scenario, Scenario::Waiting) {
                    '◐'
                } else {
                    '○'
                };
                assert!(row.contains(mark), "state lost: {row:?}");
                if matches!(scenario, Scenario::Error) {
                    assert!(row.contains('✗'), "failure lost: {row:?}");
                }
                if matches!(scenario, Scenario::Uncertain) {
                    assert!(
                        row.trim_end().ends_with('u') && frame.contains("u=unverified"),
                        "cleanup lost: {row:?}"
                    );
                }
                if slot.agent == count - 1 {
                    assert!(
                        row.contains(&format!("{}:second-recorded-identity", count + 1)),
                        "saved number lost: {row:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn dense_selection_marker_stays_on_the_centered_board() {
    let mut board_view = view(20, Scenario::Uncertain);
    board_view.agents[1].login = "documentation-bot".to_owned();
    let mut board = Board::new(Fixed(board_view));
    let buffer = draw(&mut board, 200, 18);
    let y = board.layout().listed[0].rect.y;
    assert_eq!(buffer.cell((20, y)).expect("board edge").symbol(), "▸");
    assert_eq!(buffer.cell((0, y)).expect("left margin").symbol(), " ");
}
