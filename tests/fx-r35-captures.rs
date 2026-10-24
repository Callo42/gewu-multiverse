//! fx-r35 renderer captures. Run with `GEWU_FX35_CAPTURE_DIR` set to write plain and
//! colour frames for the reviewed states and layout matrix.
//!
//! The execution sandbox on this host denies `/dev/ptmx`, so these are produced by the
//! product's real renderer with ratatui's `TestBackend`; the separately named
//! `fx-r35-live-engine` tests are the actual live process evidence. Each `.meta.txt`
//! says `renderer-testbackend` so the two are never confused.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gewu_multiverse::board::Board;
use gewu_multiverse::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, Provisioning, SessionEnd, SessionState, UpgradeView,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier};

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

#[derive(Clone, Copy)]
enum State {
    Working,
    Waiting,
    Stopped,
    Error,
    Uncertain,
    UnverifiedLeftover,
    LiveLeftover,
}

fn agent(number: usize, state: State) -> AgentView {
    let mut agent = AgentView {
        name: format!("agent-{number}"),
        login: format!("gewu-agent-{number}"),
        provisioning: Provisioning::Ready,
        desired: Desired::Started,
        session: SessionState::Running {
            number: 1,
            since: SystemTime::now(),
        },
        sessions_since_start: 1,
        runs_total: number as u64 + 2,
        recent: vec![SessionEnd::Succeeded],
        instruction_chars: 32,
        instruction_first_line: Some("work the assigned issue".to_owned()),
        instruction: Some("work the assigned issue\n".to_owned()),
        instruction_lines: 1,
        activity: Some(format!("opened pull request #{number}")),
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
    match state {
        State::Working => {
            agent.current_tool = Some("shell".to_owned());
            agent.current_tool_seconds = 7;
        }
        State::Waiting => {
            agent.session = SessionState::Restarting {
                next_at: SystemTime::now() + Duration::from_secs(20),
                last: SessionEnd::NoWork {
                    reason: "no open issue is assigned".to_owned(),
                },
            };
            agent.last_result = Some("no open issue is assigned".to_owned());
            agent.last_result_is_work = false;
            agent.no_assigned_work = true;
        }
        State::Stopped => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Idle;
        }
        State::Error => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Idle;
            agent.recent = vec![SessionEnd::Failed {
                reason: "the model call failed (server_error): the model answered HTTP 500"
                    .to_owned(),
            }];
            agent.last_result = None;
        }
        State::Uncertain => {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Blocked {
                reason: "cleanup unverified: the run started but its process records are lost; cleanup cannot be verified".to_owned(),
            };
            agent.leftover = Some(
                "cleanup unverified: the run started but its process records are lost; cleanup cannot be verified".to_owned(),
            );
        }
        State::UnverifiedLeftover => {
            agent.desired = Desired::Stopped;
            let reason = "unverified leftover: a live process group (pgid 4242) cannot be verified as this run's; it was not signalled";
            agent.session = SessionState::Blocked {
                reason: reason.to_owned(),
            };
            agent.leftover = Some(reason.to_owned());
        }
        State::LiveLeftover => {
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

fn view(count: usize, states: &[State]) -> BoardView {
    let agents = (0..count)
        .map(|index| agent(index + 1, states[index % states.len()]))
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

fn plain(buffer: &Buffer) -> String {
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

fn colour(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    let mut current: Option<(Color, Color, Modifier)> = None;
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = buffer.cell((x, y)).expect("a cell");
            let style = (cell.fg, cell.bg, cell.modifier);
            if current != Some(style) {
                out.push_str("\u{1b}[0m");
                out.push_str(&sgr(cell.fg, cell.bg, cell.modifier));
                current = Some(style);
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\u{1b}[0m\n");
        current = None;
    }
    out
}

fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = Vec::new();
    if modifier.contains(Modifier::BOLD) {
        codes.push("1".to_owned());
    }
    if modifier.contains(Modifier::DIM) {
        codes.push("2".to_owned());
    }
    if modifier.contains(Modifier::ITALIC) {
        codes.push("3".to_owned());
    }
    if modifier.contains(Modifier::UNDERLINED) {
        codes.push("4".to_owned());
    }
    if modifier.contains(Modifier::REVERSED) {
        codes.push("7".to_owned());
    }
    if let Some(code) = colour_code(fg, false) {
        codes.push(code);
    }
    if let Some(code) = colour_code(bg, true) {
        codes.push(code);
    }
    format!("\u{1b}[{}m", codes.join(";"))
}

fn colour_code(colour: Color, background: bool) -> Option<String> {
    let base = if background { 40 } else { 30 };
    Some(match colour {
        Color::Reset => return None,
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => (base + 60).to_string(),
        Color::LightRed => (base + 61).to_string(),
        Color::LightGreen => (base + 62).to_string(),
        Color::LightYellow => (base + 63).to_string(),
        Color::LightBlue => (base + 64).to_string(),
        Color::LightMagenta => (base + 65).to_string(),
        Color::LightCyan => (base + 66).to_string(),
        Color::White => (base + 67).to_string(),
        Color::Rgb(r, g, b) => {
            let prefix = if background { 48 } else { 38 };
            format!("{prefix};2;{r};{g};{b}")
        }
        Color::Indexed(index) => {
            let prefix = if background { 48 } else { 38 };
            format!("{prefix};5;{index}")
        }
    })
}

fn save(name: &str, board: &mut Board, width: u16, height: u16) {
    let Ok(dir) = std::env::var("GEWU_FX35_CAPTURE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    fs::create_dir_all(&dir).expect("capture dir");
    let buffer = draw(board, width, height);
    fs::write(dir.join(format!("{name}.txt")), plain(&buffer)).expect("plain capture");
    fs::write(dir.join(format!("{name}.ansi.txt")), colour(&buffer)).expect("colour capture");
    fs::write(
        dir.join(format!("{name}.meta.txt")),
        format!(
            "size {width}x{height}\nscreen {:?}\nsource renderer-testbackend (not a live terminal capture); live TUI proof is separate evidence\n",
            board.screen()
        ),
    )
    .expect("meta capture");
}

#[test]
fn capture_the_reviewed_state_matrix() {
    if std::env::var_os("GEWU_FX35_CAPTURE_DIR").is_none() {
        return;
    }
    let mixed = [
        State::Working,
        State::Waiting,
        State::Stopped,
        State::Error,
        State::Uncertain,
        State::UnverifiedLeftover,
        State::LiveLeftover,
    ];
    for (width, height) in [(72, 18), (80, 24), (100, 30), (120, 40), (200, 60)] {
        for count in [1, 4, 7, 12] {
            let mut board = Board::new(Fixed(view(count, &mixed)));
            save(
                &format!("r35-board-{width}x{height}-{count}-mixed"),
                &mut board,
                width,
                height,
            );
            for (suffix, event) in [("event", true), ("no-event", false)] {
                let mut quit = view(count, &[State::Working]);
                if !event {
                    quit.forgejo.events.clear();
                }
                let mut board = Board::new(Fixed(quit));
                board.on_key(KeyEvent::from(KeyCode::Char('q')));
                save(
                    &format!("r35-quit-{width}x{height}-{count}-{suffix}"),
                    &mut board,
                    width,
                    height,
                );
            }
        }
    }
    // The five accepted states on the Agent screen at the two floors and one roomy
    // size.
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        for state in [
            State::Working,
            State::Waiting,
            State::Stopped,
            State::Error,
            State::Uncertain,
            State::UnverifiedLeftover,
            State::LiveLeftover,
        ] {
            let mut board = Board::new(Fixed(view(1, &[state])));
            board.on_key(KeyEvent::from(KeyCode::Enter));
            save(
                &format!("r35-agent-{width}x{height}-{}", state_name(state)),
                &mut board,
                width,
                height,
            );
        }
    }
}

#[test]
fn capture_mixed_recorded_identity_boards() {
    if std::env::var_os("GEWU_FX35_CAPTURE_DIR").is_none() {
        return;
    }
    for state in [State::Waiting, State::Error, State::Uncertain] {
        let mut two_view = view(12, &[state]);
        two_view.agents[1].login = "documentation-bot".to_owned();
        two_view.agents[5].login = "second-recorded-identity".to_owned();
        let mut board = Board::new(Fixed(two_view));
        save(
            &format!("r35-mixed-72x18-12-{}", state_name(state)),
            &mut board,
            72,
            18,
        );

        let mut dense_view = view(12, &[state]);
        for (index, login) in [
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
        ]
        .iter()
        .enumerate()
        {
            dense_view.agents[index].login = (*login).to_owned();
        }
        let mut board = Board::new(Fixed(dense_view));
        save(
            &format!("r35-dense-72x18-12-{}", state_name(state)),
            &mut board,
            72,
            18,
        );
    }
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Working => "working",
        State::Waiting => "waiting",
        State::Stopped => "stopped",
        State::Error => "error",
        State::Uncertain => "uncertain",
        State::UnverifiedLeftover => "unverified-leftover",
        State::LiveLeftover => "live-leftover",
    }
}

#[test]
fn capture_the_version_tag_upgrade_screen() {
    if std::env::var_os("GEWU_FX35_CAPTURE_DIR").is_none() {
        return;
    }
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let mut upgrade = view(2, &[State::Stopped]);
        upgrade.upgrade = Some(UpgradeView {
            from_version: "0.1.33".to_owned(),
            to_version: "0.1.34".to_owned(),
            settings: true,
            agents: 2,
            instructions: 2,
            runs: 9,
            ignored_config: false,
            release_url: format!(
                "https://github.com/Callo42/gewu-multiverse/releases/tag/v{}",
                env!("CARGO_PKG_VERSION")
            ),
            whats_new:
                "run marks name what was observed; an ordinary earlier run is stopped and its cleanup checked before the board reopens"
                    .to_owned(),
            warnings_changed: false,
        });
        let mut board = Board::new(Fixed(upgrade));
        save(
            &format!("r35-upgrade-{width}x{height}"),
            &mut board,
            width,
            height,
        );
    }
}
