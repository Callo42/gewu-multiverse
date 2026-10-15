//! The screens, rendered without a terminal.
//!
//! `ui::draw` paints whatever backend it is given, so these tests paint an in-memory
//! backend of 100 by 30 cells and read the text back. That is the same code that
//! paints the real terminal; the difference is only where the cells go.
//!
//! Run with `--nocapture` to print every screen, which is how the screen evidence in
//! the round's report was captured:
//!
//! ```text
//! cargo test --test screens -- --nocapture
//! ```

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{App, Screen, SecretTarget};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, RunSummary, new_run_id};
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The size of the in-memory terminal.
const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

const CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 2

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 2

[[agents]]
name = "checker"
model = "other-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/checker/instruction.md"
concurrency = 1
"#;

/// A fresh, empty configuration directory for one test.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!("gewu-screens-{tag}-{}-{nanos}", process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A runner that does nothing and exits cleanly, so a screen test can start an agent
/// without starting the program itself.
fn quiet_runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// An app over a configuration written by the test.
fn app(root: &Path) -> App {
    fs::write(root.join("config.toml"), CONFIG).expect("the test writes the configuration");
    App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("runner"),
    )
}

/// Draw one frame and read the cells back as text.
fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("a test terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, app))
        .expect("a frame is drawn");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            text.push_str(symbol);
        }
        text.push('\n');
    }
    text
}

/// Draw one frame at a chosen size and read the cells back as text.
fn render_at(width: u16, height: u16, app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, app))
        .expect("a frame is drawn");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            text.push_str(symbol);
        }
        text.push('\n');
    }
    text
}

/// A screen's text with only its alphanumerics kept.
///
/// A wrapped logical line is drawn as several rows separated by the box border and a
/// newline, so a plain substring test would miss it. Removing the separators lets the
/// rows be read back as the one line they are.
fn normalised(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Print a screen with a heading, so `--nocapture` reads as a transcript.
fn show(label: &str, app: &App) {
    println!("===== {label} =====");
    println!("{}", render(app));
}

#[test]
fn the_first_run_puts_the_risk_in_front_and_names_what_it_means() {
    let root = scratch("first-run");
    let app = app(&root);
    let screen = render(&app);

    assert!(app.show_acknowledgment);
    assert!(screen.contains("Before the first agent runs"), "{screen}");
    assert!(screen.contains("runs as you"), "{screen}");
    assert!(screen.contains("not a sandbox"), "{screen}");
    assert!(screen.contains("credentials file"), "{screen}");
    assert!(screen.contains("I understand, record it once"), "{screen}");
    assert!(screen.contains("refused"), "{screen}");
}

#[test]
fn the_acknowledgment_is_recorded_once_and_then_stays_out_of_the_way() {
    let root = scratch("recorded-once");
    let mut first = app(&root);
    let path = root.join("risk-acknowledgment.toml");
    assert!(!path.exists());

    first.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(path.is_file(), "the record is written on confirmation");
    assert!(first.ack_confirmed());
    assert!(!first.show_acknowledgment);

    // A second start of the same installation does not ask again.
    let second = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("recorded-once-two"),
    );
    assert!(!second.show_acknowledgment);
    assert!(!render(&second).contains("Before the first agent runs"));

    // A fresh installation, meaning an empty configuration directory, asks again.
    let fresh_root = scratch("recorded-once-fresh");
    let fresh = app(&fresh_root);
    assert!(fresh.show_acknowledgment);
}

#[test]
fn nothing_starts_an_agent_before_the_acknowledgment() {
    let root = scratch("gate");
    let mut app = app(&root);
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert!(!app.show_acknowledgment);
    assert!(!app.ack_confirmed());

    // The start key is refused, and the acknowledgment comes back on screen.
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(app.show_acknowledgment);

    // After confirming, the start is still refused, because the agent has no
    // instruction document yet — and the screen says which key writes one.
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(
        app.status.contains("no instruction document"),
        "{}",
        app.status
    );
    assert_eq!(app.supervisor.running_count(), 0);
}

#[test]
fn the_credentials_screen_shows_the_path_and_the_mode_from_disk() {
    let root = scratch("credentials");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Credentials);
    let screen = render(&app);

    let path = root.join("credentials.toml");
    assert!(path.is_file());
    assert!(screen.contains(&path.display().to_string()), "{screen}");
    assert!(screen.contains("0600"), "{screen}");
    assert!(screen.contains("never printed"), "{screen}");
    assert!(screen.contains("redacted"), "{screen}");
}

#[test]
fn every_screen_renders_and_what_is_missing_is_named_as_missing() {
    let root = scratch("screens");
    let mut app = app(&root);
    app.on_key(KeyEvent::from(KeyCode::Char('y')));

    for screen in Screen::ALL {
        app.show(screen);
        let text = render(&app);
        show(screen.title(), &app);
        assert!(!text.is_empty());
    }

    // The Forgejo screen names the URL and role and says no check has been run yet.
    app.show(Screen::Forgejo);
    let forgejo = render(&app);
    assert!(
        forgejo.contains("no role check has been run yet"),
        "Forgejo must say what is not there yet:\n{forgejo}"
    );

    // The instruction screen names the model loop rather than calling it missing.
    app.show(Screen::Instructions);
    let instructions = render(&app);
    assert!(
        instructions.contains("declared `shell` tool"),
        "{instructions}"
    );
    assert!(instructions.contains("turns"), "{instructions}");

    // The credentials screen says the key is redacted from everything written.
    app.show(Screen::Credentials);
    let credentials = render(&app);
    assert!(credentials.contains("redacted"), "{credentials}");

    // The agent list separates the two agents and shows a state for each.
    app.show(Screen::Agents);
    let agents = render(&app);
    assert!(
        agents.contains("scribe") && agents.contains("checker"),
        "{agents}"
    );
    assert!(agents.contains("idle"), "{agents}");
    assert!(agents.contains("layout, not a sandbox"), "{agents}");

    // The agent screen shows where its files are and how to edit them.
    app.show(Screen::AgentConfig);
    let agent = render(&app);
    assert!(agent.contains("agents/scribe"), "{agent}");
    assert!(agent.contains("e edit the instruction"), "{agent}");
    assert!(agent.contains("p test the"), "{agent}");
    assert!(agent.contains("max turns"), "{agent}");
    // An unsettled identity's label must not collide with its value (G-9).
    assert!(!agent.contains("identitynot"), "{agent}");
    assert!(!agent.contains("requests"), "the limit is called concurrency: {agent}");

    // The runs screen has nothing to list yet, and says so rather than claiming a run.
    app.show(Screen::Runs);
    let runs = render(&app);
    assert!(
        runs.contains("No run of a configured agent has been recorded yet"),
        "{runs}"
    );
}

#[test]
fn the_runs_screen_lists_a_recorded_run_with_its_outcome() {
    let root = scratch("runs-recorded");
    let mut app = app(&root);
    app.show_acknowledgment = false;

    // Write one run's records by hand, exactly as a real run leaves them.
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 4321, "/w"))
        .expect("a record is appended");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Succeeded,
            Some(0),
            1234,
            None,
        ))
        .expect("a record is appended");
    fs::write(run_dir.join("run.log"), "gewu-multiverse: a run\n").expect("a log is written");

    app.refresh_recent();
    app.show(Screen::Runs);
    let screen = render(&app);
    show("Runs (one record)", &app);
    assert!(screen.contains("scribe"), "{screen}");
    assert!(screen.contains(&run_id), "{screen}");
    assert!(screen.contains("succeeded"), "{screen}");
    // The notes name the log on show; the exact path is asserted through the action
    // below, because a long temporary path wraps across rows in this narrow backend.
    assert!(screen.contains("o opens the log on show"), "{screen}");

    // `o` opens the log of the run on show.
    app.on_key(KeyEvent::from(KeyCode::Char('o')));
    let action = app.take_action().expect("a view was asked for");
    assert_eq!(action.path, run_dir.join("run.log"));
}

#[test]
fn a_run_outcome_is_not_clipped_at_the_advertised_floor() {
    // T-3: at 72x18 the Runs outcome cell was cut mid-value (`succeeded, 0`) with no
    // marker, while the run-id cell in the same row was shortened with an ellipsis.
    // The outcome keeps its room at the floor, so no cell ends mid-value.
    let root = scratch("runs-floor");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 4321, "/w"))
        .expect("a record is appended");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Succeeded,
            Some(0),
            1234,
            None,
        ))
        .expect("a record is appended");
    app.refresh_recent();
    app.show(Screen::Runs);
    for (width, height) in [(72u16, 18u16), (80u16, 24u16)] {
        let screen = render_at(width, height, &app);
        assert!(
            screen.contains("succeeded, 1.2s"),
            "the outcome is clipped at {width}x{height}: {screen}"
        );
    }
}

#[test]
fn the_forgejo_screen_shows_the_configured_url_and_role_and_claims_no_check() {
    let root = scratch("forgejo");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("https://forgejo.example.org"), "{screen}");
    assert!(screen.contains("member"), "{screen}");
    assert!(
        screen.contains("no role check has been run yet"),
        "{screen}"
    );
    assert!(screen.contains("admin token"), "{screen}");
    assert!(
        screen.contains("absent (identities are supplied)"),
        "{screen}"
    );
}

#[test]
fn the_forgejo_screen_shows_a_role_check_and_an_identity_plainly() {
    use gewu_multiverse::app::{ForgejoFailure, IdentityReport};
    use gewu_multiverse::config::Role;
    use gewu_multiverse::forgejo::{Permissions, Repository, RoleCheck, Viewer};
    use gewu_multiverse::identity::{AgentIdentity, IdentityMode};

    let root = scratch("forgejo-check");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.role_check = Some(Ok(RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Admin,
        viewer: Viewer {
            login: "root".to_owned(),
            email: None,
            description: None,
            is_admin: true,
        },
        repositories: vec![
            Repository {
                full_name: "root/demo".to_owned(),
                namespace: "root".to_owned(),
                name: "demo".to_owned(),
                private: false,
                default_branch: Some("main".to_owned()),
                html_url: None,
                permissions: Permissions {
                    admin: true,
                    push: true,
                    pull: true,
                },
            },
            Repository {
                full_name: "root/private".to_owned(),
                namespace: "root".to_owned(),
                name: "private".to_owned(),
                private: true,
                default_branch: Some("main".to_owned()),
                html_url: None,
                permissions: Permissions {
                    admin: true,
                    push: true,
                    pull: true,
                },
            },
        ],
        private_visible: 1,
    }));
    app.repo_selected = 1;
    app.identity_report = Some(IdentityReport {
        agent: "scribe".to_owned(),
        result: Ok(AgentIdentity {
            agent: "scribe".to_owned(),
            login: "gewu-scribe".to_owned(),
            email: "gewu-scribe@agents.invalid".to_owned(),
            mode: IdentityMode::Managed,
            created: true,
            created_at: Some(gewu_multiverse::time::now_unix_ms() / 1000),
        }),
    });
    app.show(Screen::Forgejo);
    let screen = render(&app);
    show("Forgejo (checked)", &app);
    assert!(screen.contains("administrative view"), "{screen}");
    assert!(screen.contains("root/private"), "{screen}");
    assert!(screen.contains("private"), "{screen}");
    assert!(screen.contains("root sees 2 repositories"), "{screen}");
    assert!(screen.contains("gewu-scribe"), "{screen}");
    assert!(screen.contains("(created"), "{screen}");

    // A refused check names its class rather than hiding it.
    app.role_check = Some(Err(ForgejoFailure {
        class: "not_admin".to_owned(),
        message: "the token belongs to `author`".to_owned(),
    }));
    let screen = render(&app);
    assert!(screen.contains("not_admin"), "{screen}");
}

#[test]
fn a_broken_configuration_is_a_message_on_screen_rather_than_a_panic() {
    let root = scratch("broken");
    fs::write(
        root.join("config.toml"),
        "[forgejo]\nurl = \"forgejo.example.org\"\nrole = \"member\"\n",
    )
    .expect("the test writes the configuration");
    let mut app = App::with_runner(Layout::rooted_at(root), quiet_runner("broken-runner"));
    app.show_acknowledgment = false;
    let screen = render(&app);
    assert!(screen.contains("forgejo.url"), "{screen}");
    assert!(screen.contains("http://"), "{screen}");
}

#[test]
fn the_instruction_screen_shows_the_files_on_disk_and_the_editing_keys() {
    let root = scratch("instruction");
    let mut app = app(&root);
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "write a note\n").expect("the instruction is written");
    fs::write(paths.steps_file(), "# nothing yet\n").expect("the steps file is written");
    fs::create_dir_all(paths.skills_dir().join("alpha")).expect("a skill directory");
    fs::write(
        paths.skills_dir().join("alpha").join("SKILL.md"),
        "# alpha\n",
    )
    .expect("a skill manifest");

    app.show(Screen::Instructions);
    let screen = render(&app);
    show("Instruction", &app);
    assert!(screen.contains("13 bytes"), "{screen}");
    assert!(screen.contains("alpha"), "{screen}");
    assert!(screen.contains("declared steps"), "{screen}");
}

#[test]
fn the_endpoint_test_shows_a_success_and_a_clear_failure_with_its_class() {
    use gewu_multiverse::test_stub::{Stub, StubReply};

    let completion = r#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;

    // A success, against a local stub.
    let good = Stub::start(vec![StubReply::json(completion)]);
    let root = scratch("endpoint-ok");
    fs::write(
        root.join("config.toml"),
        CONFIG.replace("https://model.example.org/v1", &good.url()),
    )
    .expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner("endpoint-ok"));
    app.show_acknowledgment = false;
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
    app.selected = 0;
    app.on_key(KeyEvent::from(KeyCode::Char('p')));
    app.wait_for_probe();
    let test = app.endpoint_test.clone().expect("an endpoint test result");
    assert!(test.result.is_ok(), "{}", app.status);
    assert!(
        app.status.contains("endpoint test for `scribe`: ok"),
        "{}",
        app.status
    );
    assert!(!app.status.contains("gw-fixture-key"), "{}", app.status);
    assert_eq!(good.requests().len(), 1, "one minimal call was made");

    // A clear failure: the stub refuses the key.
    let bad = Stub::start(vec![StubReply::status(401, r#"{"error":"no"}"#)]);
    let root = scratch("endpoint-bad");
    fs::write(
        root.join("config.toml"),
        CONFIG.replace("https://model.example.org/v1", &bad.url()),
    )
    .expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("endpoint-bad"),
    );
    app.show_acknowledgment = false;
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
    app.selected = 0;
    app.on_key(KeyEvent::from(KeyCode::Char('p')));
    app.wait_for_probe();
    let test = app.endpoint_test.clone().expect("an endpoint test result");
    match &test.result {
        Err(failure) => {
            assert_eq!(failure.class, "bad_key");
            assert!(failure.message.contains("401"), "{}", failure.message);
        }
        Ok(summary) => panic!("expected a failure, got {summary}"),
    }
    assert!(app.status.contains("bad_key"), "{}", app.status);
    assert!(!app.status.contains("gw-fixture-key"), "{}", app.status);

    // The agent screen shows the failure plainly.
    app.show(Screen::AgentConfig);
    let screen = render(&app);
    assert!(screen.contains("endpoint test"), "{screen}");
    assert!(screen.contains("bad_key"), "{screen}");
}

#[test]
fn an_agent_in_systemd_mode_is_shown_as_supervised_by_a_user_unit() {
    let root = scratch("systemd-mode");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let agent = app.ensure_agent("scribe").expect("the agent is prepared");
    gewu_multiverse::systemd::write_record(
        &agent,
        &gewu_multiverse::systemd::Supervision {
            mode: gewu_multiverse::systemd::Mode::Systemd,
            ..Default::default()
        },
    )
    .expect("the record is written");
    app.reload();

    // The agent list names the mode beside the state.
    let screen = render(&app);
    show("agents, one agent in systemd mode", &app);
    assert!(screen.contains("supervision"), "{screen}");
    assert!(screen.contains("systemd"), "{screen}");
    assert!(screen.contains("user unit"), "{screen}");
    assert!(screen.contains("keeps running when this UI exits"), "{screen}");

    // The agent's own screen names the unit it will be started as.
    app.show(Screen::AgentConfig);
    let screen = render(&app);
    show("agent configuration, systemd mode", &app);
    assert!(screen.contains("user unit"), "{screen}");
    assert!(screen.contains("gewu-agent-scribe.service"), "{screen}");
    assert!(screen.contains("restarts"), "{screen}");
}

/// A configuration with no agents, so the first-contact path is exercised.
const EMPTY_CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"
"#;

/// An app over a configuration the test chooses.
fn app_with_config(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    App::with_runner(Layout::rooted_at(root.to_path_buf()), quiet_runner("form"))
}

#[test]
fn the_acknowledgment_owns_its_rows_at_eighty_columns() {
    // F-A: the modal must own every row it covers, so the panel behind it never
    // collides with its edges. At 80×24 the panel border used to survive in column 0.
    let root = scratch("ack-80");
    let app = app(&root);
    assert!(app.show_acknowledgment);
    let screen = render_at(80, 24, &app);
    let lines: Vec<&str> = screen.lines().collect();
    let title_row = lines
        .iter()
        .position(|line| line.contains("Before the first agent runs"))
        .expect("the modal is on screen");
    // Every row of the modal, down to its own bottom border, must have a blank
    // one-cell margin on the left, never a surviving panel border. The modal's height
    // follows its content, so the loop stops at the modal's own closing border rather
    // than at a hard-coded row count.
    let mut checked = 0;
    for (row, line) in lines.iter().enumerate().skip(title_row) {
        assert!(
            line.starts_with(' '),
            "the panel bled into the modal margin on row {row}: {line:?}"
        );
        checked += 1;
        if line.contains('└') {
            break;
        }
    }
    assert!(checked >= 5, "the modal's rows were not all checked");
    assert!(screen.contains("Until it is recorded"), "{screen}");
}

#[test]
fn the_empty_agents_screen_offers_the_in_product_form_by_name() {
    // F-B: with nothing configured the screen itself offers the form.
    let root = scratch("empty-form");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    let screen = render(&app);
    assert!(screen.contains("No agent is configured yet"), "{screen}");
    assert!(screen.contains("press n"), "{screen}");
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert!(app.adding_agent());
    let form = render(&app);
    assert!(form.contains("agent name"), "{form}");
    assert!(form.contains("model endpoint"), "{form}");
    assert!(form.contains("model key"), "{form}");
    assert!(form.contains("Forgejo token"), "{form}");
    // `a` reaches the same form, and is never silent.
    app.on_key(KeyEvent::from(KeyCode::Esc));
    app.on_key(KeyEvent::from(KeyCode::Char('a')));
    assert!(app.adding_agent());
}

#[test]
fn the_add_agent_form_writes_both_files_and_never_shows_a_secret() {
    let root = scratch("form-write");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    for character in "scribe".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(character)));
    }
    app.on_key(KeyEvent::from(KeyCode::Tab));
    for character in "http://127.0.0.1:9/v1".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(character)));
    }
    app.on_key(KeyEvent::from(KeyCode::Tab));
    for character in "test-model".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(character)));
    }
    app.on_key(KeyEvent::from(KeyCode::Tab));
    for character in "sk-test-secret-value".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(character)));
    }
    // The form masks the key while it is typed; the value is never echoed.
    let typed = render(&app);
    assert!(!typed.contains("sk-test-secret-value"), "{typed}");
    assert!(typed.contains('*'), "{typed}");

    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert!(!app.adding_agent());
    assert!(app.status.contains("created `scribe`"), "{}", app.status);
    assert!(!app.status.contains("sk-test-secret-value"), "{}", app.status);

    // The agent is listed, and both files were written.
    assert!(app.agents().iter().any(|agent| agent.name == "scribe"));
    let config = fs::read_to_string(root.join("config.toml")).expect("config readable");
    assert!(config.contains("name = \"scribe\""), "{config}");
    // F-2: the configuration file carries the same 0600 as the credentials file it
    // names secrets in.
    let config_mode = fs::metadata(root.join("config.toml"))
        .expect("config described")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(config_mode, 0o600, "the configuration is not private");
    let credentials =
        fs::read_to_string(root.join("credentials.toml")).expect("credentials readable");
    assert!(credentials.contains("sk-test-secret-value"), "{credentials}");

    // The Credentials screen names the secret; the value never reaches a screen.
    app.show(Screen::Credentials);
    let screen = render(&app);
    assert!(screen.contains("model key for `scribe`"), "{screen}");
    assert!(!screen.contains("sk-test-secret-value"), "{screen}");
}

#[test]
fn question_mark_opens_the_key_reference_and_lists_the_hidden_keys() {
    // N-1: the legend may be clipped, but the full list is one key away.
    let root = scratch("help");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('?')));
    assert!(app.show_help);
    let screen = render(&app);
    assert!(screen.contains("keys"), "{screen}");
    assert!(screen.contains("T open its newest transcript"), "{screen}");
    assert!(screen.contains("n or a open the add-agent form"), "{screen}");
    app.on_key(KeyEvent::from(KeyCode::Char('?')));
    assert!(!app.show_help);
}

#[test]
fn the_runs_screen_opens_the_transcript_as_well_as_the_log() {
    // F-H: the transcript must be reachable inside the application.
    let root = scratch("runs-transcript");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(9);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 1, "/w"))
        .expect("a record is appended");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Succeeded,
            Some(0),
            5,
            None,
        ))
        .expect("a record is appended");
    app.refresh_recent();
    app.show(Screen::Runs);
    let screen = render(&app);
    assert!(screen.contains("T opens the transcript on show"), "{screen}");
    app.on_key(KeyEvent::from(KeyCode::Char('T')));
    let action = app.take_action().expect("a view was asked for");
    assert_eq!(action.path, run_dir.join("transcript.jsonl"));
}

#[test]
fn the_key_legend_never_stops_without_naming_the_full_list() {
    // C-1 / N-1: at 80 columns the legend must end with an explicit tail, not mid-key.
    let root = scratch("legend-tail");
    let app = app(&root);
    let screen = render_at(80, 24, &app);
    let legend = screen
        .lines()
        .find(|line| line.contains("s start"))
        .expect("the action legend is on screen");
    assert!(legend.contains("? all keys"), "{legend:?}");
    assert!(
        legend.trim_end().ends_with("? all keys"),
        "the legend must end with the affordance: {legend:?}"
    );
}

#[test]
fn an_unmapped_key_says_so_rather_than_staying_silent() {
    // S-12: a press that cannot act must not read as a freeze.
    let root = scratch("unmapped-key");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('z')));
    assert!(app.status.contains("press ?"), "{}", app.status);
}

#[test]
fn the_first_run_modal_says_what_answers_it() {
    // S-11: swallowed keys while the modal is up must not be silent.
    let root = scratch("modal-swallow");
    let mut app = app(&root);
    assert!(app.show_acknowledgment);
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(app.status.contains("answer this screen"), "{}", app.status);
}

#[test]
fn a_credentials_file_that_cannot_be_read_is_not_reported_as_empty() {
    // S-9: a parse failure shown as "nothing yet" is a lie a person acts on.
    let root = scratch("broken-credentials");
    let mut app = app(&root);
    fs::write(root.join("credentials.toml"), "this is not toml =\n")
        .expect("the broken file is written");
    app.reload();
    app.show_acknowledgment = false;
    app.show(Screen::Credentials);
    let screen = render(&app);
    assert!(screen.contains("not ready"), "{screen}");
    assert!(!screen.contains("nothing yet"), "{screen}");
}

#[test]
fn the_empty_runs_screen_names_how_a_run_starts() {
    // C-8: a literal user comes to Runs and needs the step that makes a run.
    let root = scratch("runs-empty-hint");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Runs);
    let screen = render(&app);
    assert!(screen.contains("Press s on the Agents screen"), "{screen}");
}

#[test]
fn the_agents_screen_defines_the_words_the_list_uses() {
    // C-7: `concurrency` and `max turns` are used and must be defined.
    let root = scratch("agents-definitions");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let screen = render(&app);
    assert!(screen.contains("concurrency is how many"), "{screen}");
}

#[test]
fn the_first_screen_defines_an_agent() {
    // C-9: the whole product hangs on a word the first screen never defines.
    let root = scratch("first-screen-definition");
    let app = app(&root);
    let screen = render(&app);
    assert!(screen.contains("An agent is a program"), "{screen}");
}

#[test]
fn the_selected_repository_is_marked_in_text() {
    // S-8 / C-10: selection must survive a monochrome capture.
    use gewu_multiverse::config::Role;
    use gewu_multiverse::forgejo::{Permissions, Repository, RoleCheck, Viewer};
    let root = scratch("repo-marker");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.role_check = Some(Ok(RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "uxmember".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: vec![
            Repository {
                full_name: "uxmember/demo-private".to_owned(),
                namespace: "uxmember".to_owned(),
                name: "demo-private".to_owned(),
                private: true,
                default_branch: None,
                html_url: None,
                permissions: Permissions::default(),
            },
            Repository {
                full_name: "uxmember/demo-public".to_owned(),
                namespace: "uxmember".to_owned(),
                name: "demo-public".to_owned(),
                private: false,
                default_branch: None,
                html_url: None,
                permissions: Permissions::default(),
            },
        ],
        private_visible: 1,
    }));
    app.show(Screen::Forgejo);
    app.move_repo_selection(1);
    let screen = render(&app);
    assert!(screen.contains("> uxmember/demo-public"), "{screen}");
    assert!(!screen.contains("> uxmember/demo-private"), "{screen}");
}

const SIX_AGENTS: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "a1"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a1/instruction.md"
concurrency = 1

[[agents]]
name = "a2"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a2/instruction.md"
concurrency = 1

[[agents]]
name = "a3"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a3/instruction.md"
concurrency = 1

[[agents]]
name = "a4"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a4/instruction.md"
concurrency = 1

[[agents]]
name = "a5"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a5/instruction.md"
concurrency = 1

[[agents]]
name = "a6"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/a6/instruction.md"
concurrency = 1
"#;

#[test]
fn the_agents_list_never_hides_a_row_the_selection_can_reach() {
    // X-1: more agents than rows must not vanish silently, and the selection must
    // never travel onto a row that is not drawn.
    let root = scratch("agents-window");
    let mut app = app_with_config(&root, SIX_AGENTS);
    app.show_acknowledgment = false;

    // The size the product calls usable draws at least one agent row; one size below
    // says the window is too small instead of drawing a header over nothing.
    let small = render_at(72, 18, &app);
    assert!(small.contains("a1"), "no agent row at 72x18: {small}");
    let below = render_at(72, 17, &app);
    assert!(below.contains("too small"), "{below}");

    // Six agents do not fit; the panel says how many there are.
    let wide = render_at(80, 24, &app);
    assert!(wide.contains("of 6"), "the count is missing: {wide}");

    // Walk the selection to the last agent; it must be drawn, and marked.
    for _ in 0..5 {
        app.move_agent_selection(1);
    }
    assert_eq!(app.selected, 5);
    let scrolled = render_at(80, 24, &app);
    assert!(scrolled.contains("a6"), "the selected row is hidden: {scrolled}");
    assert!(scrolled.contains(">  a6"), "the selected row is not marked: {scrolled}");
}

#[test]
fn the_selected_agent_row_is_marked_in_text() {
    // X-2: the selection must survive a monochrome capture.
    let root = scratch("agents-marker");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.move_agent_selection(1);
    let screen = render(&app);
    assert!(screen.contains(">  checker"), "{screen}");
    assert!(!screen.contains(">  scribe"), "{screen}");
}

#[test]
fn the_running_count_is_in_the_footer_on_every_screen() {
    // X-4: \"what is running\" must not be a fact only the hub knows.
    let root = scratch("footer-running");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    for screen in [
        Screen::AgentConfig,
        Screen::Instructions,
        Screen::Forgejo,
        Screen::Credentials,
        Screen::Runs,
    ] {
        app.show(screen);
        let text = render(&app);
        assert!(
            text.contains("Running: 0 of 2"),
            "{screen:?} has no running count: {text}"
        );
    }
}

#[test]
fn the_add_agent_form_says_which_token_the_forgejo_field_is() {
    // X-3: one form, two tokens; the field must say which one it takes.
    let root = scratch("form-role-token");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    let form = render(&app);
    assert!(form.contains("Forgejo role token"), "{form}");
    assert!(form.contains("agent's own token"), "{form}");
}

#[test]
fn the_identity_and_its_mode_are_on_the_forgejo_screen_persistently() {
    // X-3: the mode must not live only in a transient status line.
    let root = scratch("identity-mode");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    gewu_multiverse::credentials::record_agent_identity(
        &app.credentials_path,
        "scribe",
        None,
        "gewu-scribe",
        "gewu-scribe@agents.invalid",
        "managed",
        None,
    )
    .expect("the identity is recorded");
    app.reload();
    app.show(Screen::Forgejo);
    let screen = render(&app);
    // F6: one shape on every surface — login, email, agent and mode — and it names
    // the agent, not only the account (F1). N-2: the space between the login and the
    // email is a no-break space, so a wrap can never eat it and glue the two.
    assert!(
        screen.contains("gewu-scribe\u{a0}<gewu-scribe@agents.invalid> as `scribe` (managed)"),
        "{screen}"
    );
    assert!(screen.contains("act as this account"), "{screen}");
}

#[test]
fn the_identity_report_survives_a_restart_and_its_creation_time_ages() {
    // H-3: the report lived only in the session that settled the identity, and
    // `(created now)` never aged. It is now rebuilt from the credentials file once the
    // role check has run, and it says when the account was made.
    use gewu_multiverse::config::Role;
    use gewu_multiverse::forgejo::{RoleCheck, Viewer};
    let root = scratch("identity-persist");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    gewu_multiverse::credentials::record_agent_identity(
        &app.credentials_path,
        "scribe",
        None,
        "gewu-scribe",
        "gewu-scribe@agents.invalid",
        "managed",
        Some(gewu_multiverse::time::now_unix_ms() / 1000 - 180),
    )
    .expect("the identity is recorded");
    // A fresh session: no session report, and no role check yet.
    app.reload();
    app.identity_report = None;
    app.role_check = None;
    assert!(
        app.identity_report_for("scribe").is_none(),
        "a report before the role check has run"
    );
    // After the role check, the recorded identity is reported again.
    app.role_check = Some(Ok(RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "member".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: Vec::new(),
        private_visible: 0,
    }));
    app.show(Screen::Forgejo);
    let screen = render_at(100, 40, &app);
    assert!(screen.contains("identity for `scribe`"), "{screen}");
    assert!(
        screen.contains("gewu-scribe\u{a0}<gewu-scribe@agents.invalid> as `scribe` (managed)"),
        "{screen}"
    );
    assert!(
        screen.contains("(created 3") && screen.contains("minutes ago)"),
        "the creation time did not age: {screen}"
    );
    assert!(!screen.contains("created now"), "{screen}");
}

#[test]
fn the_runs_note_area_shows_the_reason_a_run_ended() {
    // I-2: the reason is what a person returning to Runs came for.
    let root = scratch("runs-reason");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 4321, "/w"))
        .expect("a record is appended");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Stopped,
            None,
            3300,
            Some("the run was stopped on request".to_owned()),
        ))
        .expect("a record is appended");
    app.refresh_recent();
    app.show(Screen::Runs);
    let screen = render(&app);
    assert!(screen.contains("the run was stopped on request"), "{screen}");
}

#[test]
fn the_runs_selection_is_marked_in_text_too() {
    // X-2 on the other list this persona lives in.
    let root = scratch("runs-marker");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    for (at, agent) in ["scribe", "checker"].iter().enumerate() {
        let run_id = new_run_id(at as u64 + 1);
        let run_dir = root.join("state").join(agent).join(&run_id);
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
        transcript
            .append(&Record::run_started(&run_id, agent, 1, "/w"))
            .expect("a record is appended");
        transcript
            .append(&Record::run_finished(&run_id, RunOutcome::Succeeded, Some(0), 1, None))
            .expect("a record is appended");
    }
    app.refresh_recent();
    app.show(Screen::Runs);
    app.move_run_selection(1);
    assert_eq!(app.run_selected, 1);
    let screen = render(&app);
    assert!(screen.contains("> "), "{screen}");
    assert!(screen.contains("checker"), "{screen}");
}

#[test]
fn the_instruction_screen_names_an_interrupted_editors_emergency_save() {
    // I-3: the words survive in the editor's own file; the product must say so.
    let root = scratch("instruction-backup");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "spoken for\n").expect("the instruction is written");
    fs::write(paths.instruction().with_extension("md.save"), "half a paragraph\n")
        .expect("the emergency save is written");
    app.show(Screen::Instructions);
    let screen = render(&app);
    assert!(screen.contains("not saved"), "{screen}");
    assert!(screen.contains("instruction.md.save"), "{screen}");
}

#[test]
fn an_instruction_holding_only_the_placeholder_reads_as_the_empty_state() {
    // I-4: the template is not an instruction the user wrote.
    let root = scratch("instruction-placeholder");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(
        paths.instruction(),
        format!("{}\n", gewu_multiverse::agent_run::INSTRUCTION_PLACEHOLDER),
    )
    .expect("the template is written");
    app.show(Screen::Instructions);
    let screen = render(&app);
    assert!(screen.contains("holds only the guidance line"), "{screen}");
    assert!(!screen.contains("11 bytes"), "{screen}");
}

#[test]
fn the_removal_confirmation_only_promises_files_that_exist() {
    // I-6: an interrupted first edit leaves no directory; the sentence must say so.
    let root = scratch("remove-wording");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('d')));
    let no_files = render(&app);
    assert!(no_files.contains("has no files"), "{no_files}");
    assert!(!no_files.contains("are left on disk"), "{no_files}");
    app.on_key(KeyEvent::from(KeyCode::Esc));

    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "typed\n").expect("the instruction is written");
    app.on_key(KeyEvent::from(KeyCode::Char('d')));
    let with_files = render(&app);
    // A path that may break at `/` can move the phrase to the next row; the promise
    // itself is what the test is about.
    assert!(with_files.contains("left on disk"), "{with_files}");
}

/// One repository, as a role check reports it, for the windowing tests.
fn repository(full_name: &str, private: bool) -> gewu_multiverse::forgejo::Repository {
    let (namespace, name) = full_name.split_once('/').expect("namespace/name");
    gewu_multiverse::forgejo::Repository {
        full_name: full_name.to_owned(),
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        private,
        default_branch: None,
        html_url: None,
        permissions: gewu_multiverse::forgejo::Permissions::default(),
    }
}

/// A role check with three repositories, as the wave-4 fixture had.
fn three_repository_check() -> gewu_multiverse::forgejo::RoleCheck {
    use gewu_multiverse::config::Role;
    use gewu_multiverse::forgejo::{RoleCheck, Viewer};
    RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "uxmember".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: vec![
            repository("uxadmin/admin-private", true),
            repository("uxmember/demo-private", true),
            repository("uxmember/demo-public", false),
        ],
        private_visible: 2,
    }
}

#[test]
fn the_repository_list_never_hides_a_row_the_selection_can_reach() {
    // W-1: the X-1 class on the Forgejo repository list. At the 80x24 size the list
    // is longer than the panel; the selected row must still be drawn, and the title
    // must say which rows are on show, so a repository is never hidden silently.
    let root = scratch("repo-window");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    // Seven repositories, so the list is longer than the panel at the minimum and
    // the window (not the whole list) is what is exercised.
    let mut check = three_repository_check();
    for index in 0..4 {
        check
            .repositories
            .push(repository(&format!("uxmember/extra-{index}"), false));
    }
    app.role_check = Some(Ok(check));
    app.show(Screen::Forgejo);

    let narrow = render_at(80, 24, &app);
    assert!(
        narrow.contains("of 7"),
        "the title does not say how many repositories there are: {narrow}"
    );
    // T-12: the checked list gets the panel, so more than one row is drawn even at
    // 80x24 and the explanation never takes the whole panel.
    assert!(
        narrow.matches("uxmember/").count() + narrow.matches("uxadmin/").count() >= 2,
        "only one repository row is drawn: {narrow}"
    );

    for _ in 0..2 {
        app.move_repo_selection(1);
    }
    assert_eq!(app.repo_selected, 2);
    let selected = render_at(80, 24, &app);
    assert!(
        selected.contains("> uxmember/demo-public"),
        "the selected repository is not drawn or not marked: {selected}"
    );
}

#[test]
fn the_runs_screen_says_its_start_column_is_utc() {
    // W-4: the start is UTC; the screen must not leave the user comparing it with a
    // wall clock and guessing.
    let root = scratch("runs-utc");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 4321, "/w"))
        .expect("a record is appended");
    transcript
        .append(&Record::run_finished(&run_id, RunOutcome::Succeeded, Some(0), 1000, None))
        .expect("a record is appended");
    app.refresh_recent();
    app.show(Screen::Runs);
    let screen = render(&app);
    assert!(screen.contains("started (UTC)"), "{screen}");
    // The stamp itself fits the narrow column and keeps its zone marker.
    assert!(
        screen.contains("Z"),
        "the start time has no zone marker: {screen}"
    );
}

#[test]
fn the_first_run_modal_shows_the_whole_record_path_at_eighty_columns() {
    // W-7: the acknowledgment promises the record path; at 80x24 the sentence used
    // to end without its object.
    let root = scratch("ack-path");
    let mut app = app(&root);
    app.show_acknowledgment = true;
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("The record will be written to"), "{screen}");
    // The long path wraps across two rows, but it is drawn: the sentence has its
    // object on the screen, which is what the finding asked for.
    assert!(screen.contains("risk-acknowledgment.t"), "{screen}");
}

#[test]
fn the_add_agent_form_names_the_submit_key_at_eighty_columns() {
    // P-3: at 80x24 the intro prose pushed the body's action line off the panel.
    let root = scratch("form-submit");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    let form = render_at(80, 24, &app);
    assert!(form.contains("enter writes it"), "{form}");
    assert!(form.contains("esc cancels"), "{form}");
}

#[test]
fn a_refused_role_check_names_the_configured_url() {
    // P-7: connect the failure to the address the user typed.
    use gewu_multiverse::app::ForgejoFailure;
    let root = scratch("role-url");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.role_check = Some(Err(ForgejoFailure {
        class: "not_found".to_owned(),
        message: "the Forgejo has nothing at /api/v1/user (HTTP 404)".to_owned(),
    }));
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("https://forgejo.example.org"), "{screen}");
    assert!(screen.contains("not_found"), "{screen}");
}

#[test]
fn the_key_reference_scrolls_so_every_key_survives_a_short_window() {
    // P-6: at 80x24 the help used to clip the last groups with no way to reach them.
    let root = scratch("help-scroll");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('?')));
    let first = render_at(80, 24, &app);
    assert!(first.contains("of 18"), "the range is missing: {first}");
    // Scroll to the end; the last line must be reachable, and every key with it —
    // including `u`, the Forgejo address key W32-1 added, so the reference lists it.
    let mut seen = first.clone();
    for _ in 0..20 {
        app.on_key(KeyEvent::from(KeyCode::Down));
        seen.push_str(&render_at(80, 24, &app));
    }
    assert!(
        seen.contains("esc or enter close it"),
        "the last help line is unreachable"
    );
    assert!(
        seen.contains("set the instance address"),
        "the key help does not list `u`, the Forgejo address key"
    );
}

#[test]
fn the_instruction_screen_says_what_a_run_will_send() {
    // P-4: the size on disk is not the size a run sends; the screen must say both,
    // with the reason.
    let root = scratch("instruction-sent");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(
        paths.instruction(),
        format!(
            "{}\nwrite a note\n",
            gewu_multiverse::agent_run::INSTRUCTION_PLACEHOLDER
        ),
    )
    .expect("the instruction is written");
    app.show(Screen::Instructions);
    let screen = render(&app);
    assert!(screen.contains("a run sends"), "{screen}");
    assert!(screen.contains("the guidance line is dropped"), "{screen}");
}

#[test]
fn a_paste_note_is_not_hidden_by_the_form() {
    // P-1/P-2: the sentence that says what the paste kept must stay on screen; the
    // modal must not clear the footer that carries it.
    let root = scratch("paste-note");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    for _ in 0..3 {
        app.on_key(KeyEvent::from(KeyCode::Tab));
    }
    app.on_paste("sk-first-line\nSK-SECOND-LINE");
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("first line"), "{screen}");
    assert!(screen.contains("13 characters, hidden"), "{screen}");
}

#[test]
fn a_long_status_gets_the_rows_to_be_read_whole_at_eighty_columns() {
    // T-2 / C-6 / N-6: a failed clone's cause is git's own `fatal:` line, which
    // follows the progress chatter. At the advertised minimum the footer must hold
    // both, not just the first.
    let root = scratch("status-grow");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.status = concat!(
        "clone root/alpha acted as gewu-scribe (managed) (the list came from the role token): ",
        "failed (git): Cloning into '/tmp/alpha'...\n",
        "fatal: unable to access 'http://127.0.0.1:39511/root/alpha.git/': ",
        "Failed to connect to 127.0.0.1 port 39511 after 0 ms: Couldn't connect to server"
    )
    .to_owned();
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("clone root/alpha acted as"), "{screen}");
    assert!(screen.contains("fatal: unable to access"), "{screen}");
    // Growing the footer never hides the screen behind it: the agent row is still
    // drawn at the minimum.
    assert!(screen.contains("scribe"), "{screen}");
}

#[test]
fn a_broken_configuration_names_the_key_that_reads_it_again() {
    // T-7: the recovery key belongs with the error, not only in the legend a short
    // window clips; and the diagnostic is not clipped by a fixed three-row status.
    let root = scratch("config-key");
    fs::write(root.join("config.toml"), "[[agents]\nname = \"scribe\"\n").expect("broken file");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("config-key"),
    );
    app.show_acknowledgment = false;
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("no agent list"), "{screen}");
    assert!(screen.contains("press r to read it"), "{screen}");
}

#[test]
fn a_trailing_newline_is_not_reported_as_a_dropped_guidance_line() {
    // T-3 / D-1: no guidance line was written, so none was dropped.
    let root = scratch("instruction-newline");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "Do one thing\n").expect("the instruction is written");
    app.show(Screen::Instructions);
    let screen = render(&app);
    assert!(screen.contains("trailing newline is not sent"), "{screen}");
    assert!(!screen.contains("the guidance line is dropped"), "{screen}");
}

#[test]
fn the_credentials_screen_shows_the_mode_the_file_actually_has() {
    // D-2: the screen printed the mode cached at startup while the live read refused
    // the widened file; the two facts contradicted each other.
    let root = scratch("credentials-mode");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    fs::set_permissions(
        &app.credentials_path,
        fs::Permissions::from_mode(0o644),
    )
    .expect("the credentials file is widened");
    app.show(Screen::Credentials);
    let screen = render(&app);
    assert!(screen.contains("0644"), "the real mode is not shown: {screen}");
    assert!(screen.contains("not ready"), "{screen}");
}

#[test]
fn a_one_character_secret_is_counted_in_the_singular() {
    // D-9: `1 characters` is the product's own voice with a pluralisation gutter.
    let root = scratch("one-char");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Credentials);
    app.request_secret(SecretTarget::ModelKey);
    app.on_key(KeyEvent::from(KeyCode::Char('x')));
    let screen = render(&app);
    assert!(screen.contains("1 character, hidden"), "{screen}");
    assert!(!screen.contains("1 characters"), "{screen}");
}

#[test]
fn the_model_key_prompt_names_the_agent_it_writes_for() {
    // T-4: the subject is chosen on the Agents screen and the write is not undoable.
    let root = scratch("key-subject");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Agents);
    app.move_agent_selection(1);
    app.show(Screen::Credentials);
    app.request_secret(SecretTarget::ModelKey);
    let screen = render(&app);
    assert!(screen.contains("set the model key for `checker`"), "{screen}");
    assert!(
        screen.contains("The model key for `checker` is written"),
        "{screen}"
    );
}

#[test]
fn the_forgejo_screen_never_says_placeholder() {
    // D-5: `Placeholder` is a word from inside the product, not a word for the user.
    let root = scratch("no-placeholder");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(!screen.contains("Placeholder"), "{screen}");
    assert!(screen.contains("no role check has been run yet"), "{screen}");
}

#[test]
fn the_identity_line_names_the_agent_and_the_account_i_would_create() {
    // D-4 / F1: `i` acts on a remote system. The field names the **agent** whose
    // identity is on show, and — when the tool would create the account — the account
    // itself, before the keypress.
    let root = scratch("identity-preview");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("not settled"), "{screen}");
    assert!(
        screen.contains("`scribe`"),
        "the agent is not named: {screen}"
    );
    assert!(screen.contains("v supplies the token"), "{screen}");

    // With an administrator token, the account `i` would create is named too.
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"forgejo_admin_token = \"gw-admin-fixture\"\n",
    )
    .expect("the credentials file is written");
    app.reload();
    app.show(Screen::Forgejo);
    let managed = render(&app);
    assert!(managed.contains("`gewu-scribe`"), "{managed}");
}

#[test]
fn the_agent_screen_keeps_a_long_paths_file_name_and_says_when_fields_are_below() {
    // D-7: at 80x24 the panel clipped `newest log` mid-filename and silently dropped
    // fields; the file name must survive and the panel must say more is below.
    let root = scratch("agent-overflow");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.ensure_agent("scribe").expect("the agent is prepared");
    let run_id = new_run_id(1);
    let run_dir = app.state_root().join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 1, "/w"))
        .expect("a record");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Succeeded,
            Some(0),
            10,
            None,
        ))
        .expect("a record");
    fs::write(run_dir.join(gewu_multiverse::run::LOG_FILE), "log\n").expect("a log");
    app.refresh_recent();
    app.show(Screen::AgentConfig);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("more below"), "{screen}");
    // H-5: the path must break at a `/`, never inside `run.log`, even at the floor.
    // Scroll the panel until the newest log is drawn and check the file name is whole.
    let mut scrolled = String::new();
    for _ in 0..40 {
        scrolled = render_at(80, 24, &app);
        if scrolled.contains("run.log") {
            break;
        }
        app.scroll_panel(1);
    }
    assert!(
        scrolled.contains("newest log"),
        "the newest log never came into view: {scrolled}"
    );
    assert!(
        scrolled.contains("run.log"),
        "the file name was split at the floor: {scrolled}"
    );
    // At a taller window the field and its file name are drawn whole, so the path is
    // not inherently clipped — the short window is what the title now declares.
    let tall = render_at(80, 40, &app);
    assert!(tall.contains("run.log"), "{tall}");
}


#[test]
fn the_credentials_screen_draws_its_own_keys_at_eighty_columns() {
    // O-3 / P-M1: at 80x24 the `k`/`f`/`A` line was the prose that fell off the
    // bottom. It is the row the page map requires at this state, so it must be drawn
    // at the advertised minimum — and the panel must declare whatever else it cannot.
    let root = scratch("credentials-keys-80");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    for target in [
        SecretTarget::ModelKey,
        SecretTarget::ForgejoRoleToken,
        SecretTarget::ForgejoAdminToken,
    ] {
        app.request_secret(target);
        app.on_paste("throwaway-value");
        app.on_key(KeyEvent::from(KeyCode::Enter));
    }
    fs::set_permissions(&app.credentials_path, fs::Permissions::from_mode(0o644))
        .expect("the credentials file is widened");
    app.show(Screen::Credentials);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("k set the model key"), "{screen}");
    assert!(screen.contains("f set the role token"), "{screen}");
    assert!(screen.contains("A set the administrator token"), "{screen}");
    assert!(screen.contains("more below"), "{screen}");
}

#[test]
fn the_instruction_screen_draws_what_a_run_sends_at_eighty_columns() {
    // O-3 / P-M2: "what a run will send" is a page-map must-offer; at 80x24 it used
    // to be the invisible tail of the panel.
    let root = scratch("instruction-sent-80");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "do the thing\n").expect("the instruction is written");
    app.show(Screen::Instructions);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("A run sends this instruction"), "{screen}");
    assert!(screen.contains("the trailing newline is not sent"), "{screen}");
    assert!(screen.contains("more below"), "{screen}");
}

#[test]
fn every_panel_and_note_area_that_truncates_declares_it_at_eighty_columns() {
    // O-3: the class the fifth round left open. The Agent panel was the only surface
    // that said fields were below the fold; the siblings now take the same rule, and
    // the two borderless note areas carry the sentence on their last drawn row.
    let root = scratch("panel-declares-80");
    let mut app = app(&root);
    app.show_acknowledgment = false;

    app.show(Screen::AgentConfig);
    let agent = render_at(80, 24, &app);
    assert!(agent.contains("more below"), "{agent}");

    app.show(Screen::Forgejo);
    let forgejo = render_at(80, 24, &app);
    // The unread Forgejo panel fits at 80×24 after the legend was compacted (Y-8):
    // nothing is below the fold, so there is no declaration to make, and the legend
    // must be drawn whole.
    assert!(forgejo.contains("up/down choose one"), "{forgejo}");
    assert!(forgejo.contains("c role"), "{forgejo}");

    // The Agents note area has no border to carry a title, so its own last row says so
    // when there is more below. With the acknowledgment record (C-3) the note now fits
    // at 80×24, so the declaration is exercised at the advertised floor, where the
    // same content does not fit.
    app.show(Screen::Agents);
    let agents = render_at(72, 18, &app);
    assert!(
        agents.contains("… more below; enlarge the window"),
        "{agents}"
    );

    // W36-1: the drop case. Declaring is not enough — a logical line taller than the
    // body must be drawn row by row so every row is reachable, not dropped and
    // declared. This drives every wrapped row of the Agent panel to the top of its
    // window at 72×18; a row the row window cannot show fails here.
    app.show(Screen::AgentConfig);
    render_at(72, 18, &app);
    let rows = gewu_multiverse::ui::panel_rows_text(&app);
    for (index, row) in rows.iter().enumerate() {
        let needle = normalised(row);
        if needle.is_empty() {
            continue;
        }
        app.panel_scroll = index;
        let screen = normalised(&render_at(72, 18, &app));
        assert!(
            screen.contains(&needle),
            "a wrapped row is dropped unreachably at 72×18: {row:?}"
        );
    }
}

#[test]
fn nothing_a_panel_holds_is_dropped_unreachably_at_the_advertised_floor() {
    // W36-1 (the class): the Credentials `present` list was one joined logical line;
    // with many secrets it wrapped to more rows than the panel body at 72×18, so
    // `fit_panel_lines` dropped it from the bottom and no `[`/`]` offset could bring
    // it back. The floor rule is fits / wraps whole / declares — never silently
    // drops. This drives every surface at 72×18, scrolls it from its top to its end,
    // and requires every logical line the surface holds to be drawn at some offset.
    let root = scratch("floor-drops");
    let mut config = String::from(
        "[forgejo]\nurl = \"https://forgejo.example.org\"\nrole = \"member\"\n\n\
         [limits]\nrun_timeout_secs = 30\nmax_concurrent_agents = 2\n",
    );
    for index in 0..12 {
        config.push_str(&format!(
            "\n[[agents]]\nname = \"agent-{index:02}\"\nmodel = \"model-{index:02}\"\n\
             endpoint = \"https://model.example.org/v1\"\n\
             instruction = \"agents/agent-{index:02}/instruction.md\"\nconcurrency = 1\n"
        ));
    }
    let mut app = app_with_config(&root, &config);
    app.show_acknowledgment = false;
    // Many model keys make the `present` list long — the exact state W36-1 found.
    for index in 0..12 {
        app.selected = index;
        app.request_secret(SecretTarget::ModelKey);
        app.on_paste("throwaway-value");
        app.on_key(KeyEvent::from(KeyCode::Enter));
    }
    // A little run history, so the Runs note carries a footprint and rows.
    seed_runs(&root, "agent-00", 4, 0);
    app.reload();

    for (screen, name) in [
        (Screen::Agents, "agents"),
        (Screen::AgentConfig, "agent"),
        (Screen::Instructions, "instruction"),
        (Screen::Forgejo, "forgejo"),
        (Screen::Credentials, "credentials"),
        (Screen::Runs, "runs"),
    ] {
        app.show(screen);
        // The first frame fixes the width the row wrapping is measured at.
        render_at(72, 18, &app);
        let rows = gewu_multiverse::ui::panel_rows_text(&app);
        assert_eq!(
            rows.len(),
            gewu_multiverse::ui::panel_total_lines(&app),
            "{name}: the scroll count and the drawn rows disagree"
        );
        // Drive every wrapped row to the top of its window; it must be drawn there.
        for (index, row) in rows.iter().enumerate() {
            let needle = normalised(row);
            if needle.is_empty() {
                continue;
            }
            app.panel_scroll = index;
            let screen = normalised(&render_at(72, 18, &app));
            assert!(
                screen.contains(&needle),
                "{name}: row {index} is dropped unreachably at 72×18: {row:?}"
            );
        }
    }
}

#[test]
fn the_runs_note_area_keeps_the_reason_and_both_offers_at_eighty_columns() {
    // O-3 / P-M3: the reason leads, and `o`/`T` are both named beside it; a long
    // reason used to push the `o` line off the bottom at 80x24.
    let root = scratch("runs-note-80");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_dir = root.join("state").join("scribe").join(new_run_id(1));
    app.recent = vec![RunSummary {
        agent: "scribe".to_owned(),
        run_id: new_run_id(1),
        outcome: Some(RunOutcome::Failed),
        reason: Some(
            "the model call failed (transport): the request could not be sent: \
             io: Connection refused (os error 111)"
                .to_owned(),
        ),
        duration_ms: Some(17_400),
        started_at: Some("2026-10-14T13:00:00Z".to_owned()),
        log: run_dir.join("run.log"),
        dir: run_dir,
    }];
    app.run_selected = 0;
    app.show(Screen::Runs);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("outcome:"), "{screen}");
    // The record keeps the transport's words; the screen shows the product's (O-4).
    assert!(
        screen.contains("the endpoint refused the connection"),
        "{screen}"
    );
    assert!(
        screen.contains("o opens the log on show"),
        "the `o` line is not drawn: {screen}"
    );
    assert!(
        screen.contains("T opens the transcript on show"),
        "the `T` line is not drawn: {screen}"
    );
    // M-5: the pager's own way out is named on the screen, before `o`/`T` is pressed.
    assert!(screen.contains("leave the pager with q"), "{screen}");
}

#[test]
fn the_add_agent_form_draws_every_field_and_the_cursor_at_eighty_columns() {
    // M-1: at 80x24 the form drew four fields and let the cursor walk onto the two
    // Forgejo rows without drawing them. Every field and the cursor's own row must be
    // on the screen, and a window too short to hold the form must say so.
    let root = scratch("form-fields-80");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    let first = render_at(80, 24, &app);
    for label in ["agent name", "model endpoint", "model id", "model key"] {
        assert!(first.contains(label), "the field {label} is not drawn: {first}");
    }
    // Walk to the last field: the cursor must take the view with it.
    for _ in 0..5 {
        app.on_key(KeyEvent::from(KeyCode::Down));
    }
    let last = render_at(80, 24, &app);
    assert!(
        last.contains("> Forgejo role token"),
        "the cursor's own field is not drawn: {last}"
    );
    // LIB-1: the form declares its overflow with the same vocabulary every other
    // bordered surface uses, fitted to the width. Walking to the last field scrolls
    // the window, so the title names the direction (`↑ above`) and the short form of
    // the declaration is what fits at 80 columns.
    assert!(
        last.contains("↑ above; more below") || last.contains("more below; enlarge the window"),
        "the form does not declare its scrolled window: {last}"
    );
}

#[test]
fn the_endpoint_test_failure_names_its_cause_in_the_product_words_at_eighty_columns() {
    // O-2: the footer repeated only the class, so the cause (and the path the D-3 fix
    // added) was below the fold. O-4: `io:` and `(os error 111)` are the transport
    // library's words, not the product's.
    const CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "http://127.0.0.1:1/v1/chat/completions"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#;
    let root = scratch("endpoint-failure-80");
    let mut app = app_with_config(&root, CONFIG);
    app.show_acknowledgment = false;
    app.request_secret(SecretTarget::ModelKey);
    app.on_paste("sk-not-a-real-credential");
    app.on_key(KeyEvent::from(KeyCode::Enter));
    // C-1: a status sentence belongs to the screen that made it, so the test asks for
    // the test on the Agent screen, where a person pressing `p` is standing — the way
    // a user drives it — rather than carrying a stale sentence across a screen change.
    app.show(Screen::AgentConfig);
    app.request_endpoint_test();
    app.wait_for_probe();
    assert!(
        app.status.contains("the endpoint refused the connection"),
        "the footer does not carry the cause: {}",
        app.status
    );
    assert!(
        !app.status.contains("os error"),
        "the transport's errno is still on the status line: {}",
        app.status
    );
    let screen = render_at(80, 24, &app);
    // The footer carries it at the minimum (the sentence wraps, so the whole phrase is
    // not contiguous); the transport's errno is nowhere on the screen.
    assert!(screen.contains("endpoint test for `scribe`"), "{screen}");
    assert!(!screen.contains("os error"), "{screen}");
}

#[test]
fn a_stored_transport_reason_is_shown_in_the_product_words() {
    // O-4: the record keeps the transport's sentence, but every screen that shows it
    // translates it; the errno gutter is not the product's vocabulary.
    let root = scratch("product-words-80");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_dir = root.join("state").join("scribe").join(new_run_id(1));
    app.recent = vec![RunSummary {
        agent: "scribe".to_owned(),
        run_id: new_run_id(1),
        outcome: Some(RunOutcome::Failed),
        reason: Some(
            "the model call failed (transport): io: Connection refused (os error 111)"
                .to_owned(),
        ),
        duration_ms: Some(0),
        started_at: Some("2026-10-14T13:00:00Z".to_owned()),
        log: run_dir.join("run.log"),
        dir: run_dir,
    }];
    app.run_selected = 0;
    app.show(Screen::Agents);
    let agents = render_at(80, 24, &app);
    assert!(
        agents.contains("the endpoint refused the connection"),
        "{agents}"
    );
    assert!(!agents.contains("os error 111"), "{agents}");
    app.show(Screen::Runs);
    let runs = render_at(80, 24, &app);
    assert!(!runs.contains("os error 111"), "{runs}");
}

#[test]
fn the_forgejo_title_never_clips_its_overflow_declaration_at_eighty_columns() {
    // Y-1: with a repository range present, the title carried both the range and the
    // declaration at 80×24 and the terminal clipped the declaration to `more bel`.
    // The rule: the declaration is never the part that is clipped. At a short window
    // the shortened screen name keeps the range *and* the whole declaration.
    let root = scratch("forgejo-title");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let mut check = three_repository_check();
    for index in 0..4 {
        check
            .repositories
            .push(repository(&format!("uxmember/extra-{index}"), false));
    }
    app.role_check = Some(Ok(check));
    app.show(Screen::Forgejo);

    // A short window (18 rows, the advertised floor's height) forces the panel to
    // declare. The declaration is never the part that is clipped; with the panel now
    // scrollable (A-3) the range moves to a line above the list rather than sharing a
    // title that would have to clip one of them.
    let short = render_at(80, 18, &app);
    let title_line = short
        .lines()
        .find(|line| line.contains("┌ Forgejo"))
        .unwrap_or_default();
    assert!(
        title_line.contains("more below; enlarge the window"),
        "the declaration is clipped or missing: {title_line:?}"
    );
    assert!(
        !title_line.contains("more bel ") && !title_line.ends_with("more bel"),
        "the declaration is clipped: {title_line:?}"
    );
    // The range is not lost: a few `]` presses bring the line that names it, above
    // the list, into the window.
    for _ in 0..3 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    let scrolled = render_at(80, 18, &app);
    assert!(
        scrolled.contains("repositories 1–7 of 7"),
        "the repository range is not reachable: {scrolled}"
    );
}

#[test]
fn the_forgejo_legend_keeps_each_phrase_whole() {
    // Y-8: the legend used to be one wrapped line, and the panel cut it mid-phrase at
    // `v set the agent's`. Each phrase now has a row of its own, and `v` keeps its
    // object.
    let root = scratch("forgejo-legend");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("v agent token"), "{screen}");
    assert!(screen.contains("A admin token"), "{screen}");
    // No drawn row ends with a possessive that has lost its noun.
    for line in screen.lines() {
        let trimmed = line.trim_end_matches(['│', ' ']);
        assert!(
            !trimmed.ends_with("agent's") && !trimmed.ends_with("the agent's own"),
            "a legend phrase is clipped mid-phrase: {trimmed:?}"
        );
    }
}

#[test]
fn a_panel_scrolls_to_its_last_line_and_the_arrows_are_not_swallowed() {
    // Y-2: the panel declared `more below` and the arrows did nothing at all. On a
    // screen with no list, the arrows and page keys now move the panel, `[`/`]` move
    // it on every screen, and every press says where the panel is.
    let root = scratch("panel-scroll");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Credentials);
    let first = render_at(80, 24, &app);
    assert!(first.contains("more below"), "{first}");

    // The arrows act on the panel here (no list on this screen).
    app.on_key(KeyEvent::from(KeyCode::PageDown));
    assert!(app.panel_scroll > 0, "PageDown did not move the panel");
    assert!(
        app.status.contains("scrolled") || app.status.contains("at its end"),
        "PageDown was swallowed in silence: {}",
        app.status
    );
    app.on_key(KeyEvent::from(KeyCode::Home));
    assert_eq!(app.panel_scroll, 0, "Home did not return to the top");

    // The last line, which the fold hid, is reachable with End.
    app.on_key(KeyEvent::from(KeyCode::End));
    let last = render_at(80, 24, &app);
    assert!(
        last.contains("redacted from every transcript"),
        "the tail of the panel is unreachable: {last}"
    );
    assert!(app.status.contains("at its end"), "{}", app.status);

    // `]` scrolls the panel on a screen whose arrows belong to a list.
    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char(']')));
    assert!(app.panel_scroll > 0, "`]` did not scroll the note area");
}

#[test]
fn the_runs_note_leaves_the_list_more_than_one_row_at_eighty_columns() {
    // Y-3: the note area took two thirds of the height and the run list had one row.
    // A person who comes back to survey several runs must see several.
    let root = scratch("runs-rows");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.recent = (0..5)
        .map(|index| {
            let run_dir = root.join("state").join("scribe").join(new_run_id(index));
            RunSummary {
                agent: "scribe".to_owned(),
                run_id: new_run_id(index),
                outcome: Some(RunOutcome::Failed),
                reason: Some("the model call failed (transport): the endpoint refused the connection".to_owned()),
                duration_ms: Some(1_000 + index),
                started_at: Some("2026-10-14T13:00:00Z".to_owned()),
                log: run_dir.join("run.log"),
                dir: run_dir,
            }
        })
        .collect();
    app.run_selected = 0;
    app.show(Screen::Runs);
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("1–1 of 5"),
        "the run list is one row tall again: {screen}"
    );
    assert!(
        screen.contains("of 5"),
        "the window does not say how many runs there are: {screen}"
    );
    // The reason and both offers still fit beside the list.
    assert!(screen.contains("outcome:"), "{screen}");
    assert!(screen.contains("T opens the transcript on show"), "{screen}");
    assert!(screen.contains("o opens the log on show"), "{screen}");
    assert!(screen.contains("leave the pager with q"), "{screen}");
}

#[test]
fn a_record_the_hub_had_that_is_gone_is_named_and_not_called_the_last_run() {
    // Y-4: deleting the newest record silently promoted an older failure to "the last
    // run". The hub says a record it had is gone, and calls the older record what it is.
    let root = scratch("forgotten-record");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_dir = root.join("state").join("scribe").join(new_run_id(1));
    app.recent = vec![RunSummary {
        agent: "scribe".to_owned(),
        run_id: new_run_id(1),
        outcome: Some(RunOutcome::Failed),
        reason: Some("the endpoint refused the connection".to_owned()),
        duration_ms: Some(120),
        started_at: Some("2026-10-14T13:00:00Z".to_owned()),
        log: run_dir.join("run.log"),
        dir: run_dir,
    }];
    app.forgotten_run = Some("scribe".to_owned());
    app.show(Screen::Agents);
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("a run record the hub had for `scribe` is no longer there"),
        "{screen}"
    );
    assert!(
        screen.contains("the newest run record left for `scribe`"),
        "the older failure is still called the last run: {screen}"
    );
    assert!(
        !screen.contains("the last run of `scribe` did not go well"),
        "{screen}"
    );
}

#[test]
fn a_missing_model_key_reason_reads_as_the_products_own_key_on_every_screen() {
    // E-6, P-4: a start that was queued while the key existed reaches the missing-key
    // path when its turn comes. The run's stored reason carries the credentials file
    // and `[model_keys]`; the hub note area, the Agent screen and the Runs outcome
    // note must all render the product's own key instead.
    let root = scratch("missing-key-surfaces");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let reason = "the model key cannot be read: the credentials file /tmp/example/credentials.toml records no model key for the agent `scribe`; add one under [model_keys]".to_owned();
    let run_dir = root.join("state").join("scribe").join(new_run_id(1));
    app.recent = vec![RunSummary {
        agent: "scribe".to_owned(),
        run_id: new_run_id(1),
        outcome: Some(RunOutcome::Failed),
        reason: Some(reason),
        duration_ms: Some(0),
        started_at: Some("2026-10-14T13:00:00Z".to_owned()),
        log: run_dir.join("run.log"),
        dir: run_dir,
    }];
    // The sentence wraps at 80 columns, so the test checks its two halves rather
    // than one unbroken line.
    let key_is_gone = "the model key for `scribe` is gone";
    let fix = "press 5 and then k";

    app.show(Screen::Agents);
    let hub = render_at(80, 24, &app);
    assert!(hub.contains(key_is_gone), "the hub note does not: {hub}");
    assert!(hub.contains(fix), "the hub note names no fix: {hub}");
    assert!(!hub.contains("[model_keys]"), "the hub names a TOML section: {hub}");

    app.show(Screen::AgentConfig);
    let agent = render_at(80, 40, &app);
    assert!(agent.contains(key_is_gone), "the Agent screen does not: {agent}");
    assert!(agent.contains(fix), "the Agent screen names no fix: {agent}");
    assert!(
        !agent.contains("[model_keys]"),
        "the Agent screen names a TOML section: {agent}"
    );

    app.show(Screen::Runs);
    app.run_selected = 0;
    let runs = render_at(80, 24, &app);
    assert!(runs.contains(key_is_gone), "the Runs outcome note does not: {runs}");
    assert!(
        !runs.contains("[model_keys]"),
        "the Runs outcome names a TOML section: {runs}"
    );
}

#[test]
fn the_forgejo_legend_is_always_reachable_with_an_identity_report_at_eighty_by_twenty_four() {
    // P-6: with an identity report on the tail the windowed form no longer fits at
    // 80×24, so the product draws the scrolled form. The page map now says the legend
    // is not pinned in that state; this guard proves it is one `]` away and never
    // hidden in silence.
    use gewu_multiverse::app::{ForgejoFailure, IdentityReport};
    let root = scratch("forgejo-identity-legend");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let mut check = three_repository_check();
    for index in 0..7 {
        check
            .repositories
            .push(repository(&format!("org/extra-{index}"), false));
    }
    app.role_check = Some(Ok(check));
    app.identity_report = Some(IdentityReport {
        agent: "scribe".to_owned(),
        result: Err(ForgejoFailure {
            class: "identity".to_owned(),
            message:
                "the token was refused for a reason long enough to wrap on the panel at eighty columns"
                    .to_owned(),
        }),
    });
    app.show(Screen::Forgejo);
    let mut reached = false;
    for _ in 0..40 {
        let screen = render_at(80, 24, &app);
        if screen.contains("v agent token") && screen.contains("i identity") {
            reached = true;
            break;
        }
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    assert!(
        reached,
        "the key legend is unreachable at 80×24 with an identity report"
    );
}

#[test]
fn the_footer_never_advertises_q_quit_under_an_overlay_that_owns_the_keys() {
    // E-2: the footer's `q quit` sat under the add-agent form, the secret prompt and
    // the confirmations, where `q` is a letter in a field or a cancel. The legend is
    // replaced while an overlay owns the keys, so the product never advertises a key
    // that does something else (or nothing).
    let root = scratch("footer-legend");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Agents);
    let plain = render_at(80, 24, &app);
    assert!(plain.contains("q quit"), "the plain legend lost `q quit`: {plain}");

    app.confirm_remove = Some("scribe".to_owned());
    let overlay = render_at(80, 24, &app);
    assert!(
        !overlay.contains("q quit"),
        "the footer still advertises quit under a confirmation: {overlay}"
    );
    assert!(
        overlay.contains("esc closes this overlay"),
        "the footer does not say what the keys do here: {overlay}"
    );
    assert!(
        overlay.contains("remove it"),
        "the confirmation's own action line is missing: {overlay}"
    );
}

#[test]
fn a_legal_agent_name_is_never_clipped_into_an_ambiguous_neighbour() {
    // P-2: the hub clipped every name at 14 characters with no ellipsis, so two legal
    // names that shared the first 14 read identically. The column is widened to the
    // shortest width that tells the names apart, and the shortened form keeps both
    // ends of the name.
    let root = scratch("long-names");
    let config = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 2

[[agents]]
name = "cccccccccccccc"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/cccccccccccccc/instruction.md"
concurrency = 1

[[agents]]
name = "cccccccccccccc-different"
model = "m"
endpoint = "https://m.example.org"
instruction = "agents/cccccccccccccc-different/instruction.md"
concurrency = 1
"#;
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root), quiet_runner("long-names"));
    app.show_acknowledgment = false;
    app.show(Screen::Agents);
    let hub = render_at(80, 24, &app);
    // The first name is exactly 14 characters and is drawn whole; the second is
    // shortened with an ellipsis and keeps its distinguishing tail.
    assert!(hub.contains("cccccccccccccc"), "the first name is missing: {hub}");
    assert!(
        hub.contains("fferent"),
        "the second name cannot be told from the first: {hub}"
    );
    assert!(
        hub.contains('…'),
        "the clipped name carries no marker: {hub}"
    );
}

#[test]
fn the_hub_state_column_never_clips_a_word_mid_token() {
    // E-7, P-5: the fixed state column clipped `failed (missing_key)` to
    // `failed (missing_ke` and `waiting (1 in line)` to `waiting (1 in line`, with no
    // marker. The column is sized to the longest state word it can show, and the
    // failure class for a missing key is shortened so it fits whole.
    let root = scratch("state-column");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 42, "/w"))
        .expect("a record");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Failed,
            None,
            0,
            Some(
                "the model key cannot be read: the credentials file /tmp/example/credentials.toml records no model key for the agent `scribe`; add one under [model_keys]"
                    .to_owned(),
            ),
        ))
        .expect("a record");
    app.reload();
    app.show(Screen::Agents);
    let hub = render_at(80, 24, &app);
    assert!(
        hub.contains("failed (key)"),
        "the state word is not whole: {hub}"
    );
    assert!(
        !hub.contains("failed (missing_ke"),
        "the state word is clipped mid-token: {hub}"
    );
}

#[test]
fn an_endpoint_test_is_dated_and_marked_when_the_endpoint_changed() {
    // Y-5: the result outlived the endpoint it tested and read as current. It is now
    // dated, and a result about another endpoint is marked not current.
    let root = scratch("endpoint-stale");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.endpoint_test = Some(gewu_multiverse::app::EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: "https://model.example.org/v1".to_owned(),
        model: "example-model".to_owned(),
        result: Ok("the endpoint answered: replied `ok`; 3 tokens used".to_owned()),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: gewu_multiverse::machine::mark(),
    });
    app.show(Screen::AgentConfig);
    let fresh = render_at(80, 40, &app);
    assert!(fresh.contains("tested just now"), "{fresh}");

    // The configuration now names a different endpoint: the old result is history.
    app.endpoint_test = Some(gewu_multiverse::app::EndpointTest {
        endpoint: "http://127.0.0.1:2/v1".to_owned(),
        ..app.endpoint_test.take().expect("the test result")
    });
    let stale = render_at(80, 40, &app);
    assert!(stale.contains("not current"), "{stale}");
    assert!(stale.contains("press p to test it again"), "{stale}");
}

#[test]
fn a_removal_names_the_run_records_and_the_secret_and_credentials_deletes_it() {
    // Y-6: the removal box named only the agent directory, the run records were hidden
    // and the leftover secret had no delete. The box now names all three places, and
    // `D` on Credentials deletes the secret of an agent no longer configured.
    let root = scratch("remove-leftovers");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    // Give `scribe` a model key, then remove the agent from the configuration.
    app.request_secret(SecretTarget::ModelKey);
    app.on_paste("sk-throwaway-for-this-test");
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert!(
        app.present_secret_labels()
            .iter()
            .any(|label| label.contains("model key for `scribe`"))
    );

    let state_dir = app.state_root().join("scribe");
    fs::create_dir_all(&state_dir).expect("a run record directory");
    let declared = app.selected_name().expect("the selected agent");
    app.confirm_remove = Some(declared);
    app.on_key(KeyEvent::from(KeyCode::Char('y')));

    // The leftover secret is marked, and `D` offers to delete it.
    app.show(Screen::Credentials);
    let marked = render_at(80, 24, &app);
    assert!(
        marked.contains("no agent with that name is configured"),
        "{marked}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('D')));
    let confirm = render_at(80, 24, &app);
    assert!(confirm.contains("Delete the leftover secrets"), "{confirm}");
    assert!(confirm.contains("`scribe`"), "{confirm}");
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(
        !app.present_secret_labels()
            .iter()
            .any(|label| label.contains("model key for `scribe`")),
        "the leftover secret survived: {:?}",
        app.present_secret_labels()
    );
    assert!(app.status.contains("deleted"), "{}", app.status);

    // The removal confirmation names the run records and the credentials file too.
    let remaining = app.selected_name().expect("a remaining agent");
    app.ensure_agent(&remaining).expect("the agent files");
    fs::create_dir_all(app.state_root().join(&remaining)).expect("a run record directory");
    app.confirm_remove = Some(remaining);
    app.show(Screen::Agents);
    let boxed = render_at(80, 24, &app);
    assert!(boxed.contains("run records under"), "{boxed}");
}

#[test]
fn a_newest_log_that_is_gone_is_marked() {
    // Y-7: the Agent screen named a log path for a file that had been deleted, as if it
    // were a thing. A record with no log says so.
    let root = scratch("log-gone");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let run_id = new_run_id(1);
    let run_dir = root.join("state").join("scribe").join(&run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(&run_id, "scribe", 42, "/w"))
        .expect("a record");
    transcript
        .append(&Record::run_finished(
            &run_id,
            RunOutcome::Succeeded,
            Some(0),
            7,
            None,
        ))
        .expect("a record");
    // No run.log is written.
    app.reload();
    app.show(Screen::AgentConfig);
    let screen = render_at(80, 40, &app);
    assert!(screen.contains("newest log"), "{screen}");
    assert!(screen.contains("gone, the record has no log"), "{screen}");
}

#[test]
fn a_screen_never_opens_at_another_screens_scroll_offset_and_a_scrolled_panel_says_so() {
    // A-1: the scroll offset was shared between screens — the number-key path never
    // reset it — so Instruction opened at the Agents note area's offset with its own
    // heading gone and no marker. Every screen change resets it, and a panel the user
    // *does* scroll says there is content above.
    let root = scratch("scroll-reset");
    let mut app = app(&root);
    app.show_acknowledgment = false;

    app.show(Screen::Agents);
    for _ in 0..6 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    assert!(app.panel_scroll > 0, "the Agents note area did not scroll");

    // The digit path: the offset must not follow the user to another screen.
    app.on_key(KeyEvent::from(KeyCode::Char('3')));
    assert_eq!(
        app.panel_scroll, 0,
        "the digit path carried the offset to Instruction"
    );
    let instruction = render_at(80, 24, &app);
    assert!(
        instruction.contains("on disk"),
        "Instruction did not open at its own top: {instruction}"
    );
    assert!(
        !instruction.contains("↑ more above"),
        "an unscrolled panel claims content above: {instruction}"
    );

    // A panel the user scrolls on this screen says so rather than hiding its heading.
    for _ in 0..4 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    let scrolled = render_at(80, 24, &app);
    assert!(
        scrolled.contains("↑ more above") || scrolled.contains("↑ above"),
        "a scrolled panel hides its top in silence: {scrolled}"
    );

    // `enter` into an agent, and `esc` back, reset it too.
    app.show(Screen::Agents);
    for _ in 0..6 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(app.panel_scroll, 0, "`enter` carried the offset");
    for _ in 0..4 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
    }
    app.on_key(KeyEvent::from(KeyCode::Esc));
    assert_eq!(app.panel_scroll, 0, "`esc` carried the offset");
}

#[test]
fn a_screen_shows_the_instruction_path_the_run_reads() {
    // P-3: the Agent screen echoed the configuration's `instruction` field while the
    // Instruction screen and every run read the derived agents/<name>/instruction.md.
    // A configured path with a space, outside the derived default, proves the two
    // screens now name the same file the run reads.
    let root = scratch("instruction-field");
    let config = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 2

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "my instructions/scribe.md"
concurrency = 1
"#;
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("instruction-field"),
    );
    app.show_acknowledgment = false;
    let configured = root.join("my instructions/scribe.md");
    fs::create_dir_all(configured.parent().expect("a parent")).expect("the directory");
    fs::write(&configured, "do something\n").expect("the instruction is written");
    let derived = root.join("agents/scribe/instruction.md");

    app.show(Screen::AgentConfig);
    let agent = render_at(200, 40, &app);
    assert!(
        agent.contains(&configured.display().to_string()),
        "the Agent screen does not name the configured instruction: {agent}"
    );
    assert!(
        !agent.contains(&derived.display().to_string()),
        "the Agent screen still names the ignored derived path: {agent}"
    );
    app.show(Screen::Instructions);
    let instruction = render_at(200, 40, &app);
    assert!(
        instruction.contains(&configured.display().to_string()),
        "the Instruction screen does not agree with the run: {instruction}"
    );
}

#[test]
fn the_forgejo_panel_scrolls_to_its_repository_list_and_legend_below_eighty_by_twenty_four() {
    // A-3: below 80x24 the Forgejo panel declared `more below` and `[`/`]` refused, so
    // the repository list and the key legend were unreachable and the selection moved
    // through rows that were never drawn. The panel now scrolls like every other one.
    let root = scratch("forgejo-scroll-small");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let mut check = three_repository_check();
    for index in 0..4 {
        check
            .repositories
            .push(repository(&format!("uxmember/extra-{index}"), false));
    }
    app.role_check = Some(Ok(check));
    app.show(Screen::Forgejo);

    for (width, height) in [(80u16, 18u16), (72, 18)] {
        app.panel_scroll = 0;
        let top = render_at(width, height, &app);
        assert!(
            top.contains("more below"),
            "{width}x{height}: the overflowing panel does not declare: {top}"
        );
        // Scroll to the end one row at a time; both a repository row and the key
        // legend must be drawn at some offset (they need not share a window when the
        // body is only two rows tall).
        let total = gewu_multiverse::ui::panel_total_lines(&app);
        let mut saw_repo = false;
        let mut saw_legend = false;
        for _ in 0..=total {
            let reached = render_at(width, height, &app);
            saw_repo |= reached.contains("uxmember/") || reached.contains("uxadmin/");
            saw_legend |= reached.contains("up/down choose one");
            app.on_key(KeyEvent::from(KeyCode::Char(']')));
        }
        assert!(
            saw_repo,
            "{width}x{height}: no repository row is reachable"
        );
        assert!(
            saw_legend,
            "{width}x{height}: the key legend is not reachable"
        );
        assert!(
            !app.status.contains("the panel below is declared"),
            "{width}x{height}: the refusal still names a panel that is not there: {}",
            app.status
        );
    }
}

#[test]
fn every_screen_name_survives_the_advertised_floor() {
    // A-4: at 72 columns the tab bar clipped the last name to `6 Ru`.
    let root = scratch("tab-names-72");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let screen = render_at(72, 18, &app);
    let tabs: Vec<&str> = screen.lines().take(3).collect();
    let bar = tabs.join("\n");
    for name in [
        "1 Agents",
        "2 Agent",
        "3 Instruction",
        "4 Forgejo",
        "5 Credentials",
        "6 Runs",
    ] {
        assert!(
            bar.contains(name),
            "the tab bar lost `{name}` at 72 columns: {bar}"
        );
    }
    assert!(
        !bar.contains("6 Ru ") && !bar.contains("6 Ru│") && !bar.contains("6 Ru\n"),
        "the last tab name is clipped: {bar}"
    );
}

#[test]
fn the_credentials_legend_offers_the_leftover_delete_when_one_is_shown() {
    // A-5: the screen that shows a leftover secret did not offer `D`; it lived only on
    // a help page.
    let root = scratch("credentials-leftover-legend");
    let mut leftover = app(&root);
    leftover.show_acknowledgment = false;
    let path = root.join("credentials.toml");
    fs::write(
        &path,
        "forgejo_token = \"\"\n\n[model_keys]\n\"ghost\" = \"gw-throwaway\"\n",
    )
    .expect("the credentials file is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private mode");
    leftover.show(Screen::Credentials);
    let screen = render_at(80, 24, &leftover);
    assert!(
        screen.contains("no agent with that name is configured"),
        "the leftover is not marked: {screen}"
    );
    assert!(
        screen.contains("D delete the leftover secrets"),
        "the legend does not offer the delete: {screen}"
    );

    // With no leftover, the legend stays as it was.
    let empty = scratch("credentials-no-leftover-legend");
    let mut plain = app(&empty);
    plain.show_acknowledgment = false;
    plain.show(Screen::Credentials);
    let no_leftover = render_at(80, 24, &plain);
    assert!(
        !no_leftover.contains("D delete the leftover secrets"),
        "the legend offers a delete with nothing to delete: {no_leftover}"
    );
}

/// Write `count` finished run records for one agent, so a test can cross the fifty
/// record cap or make a footprint worth measuring.
fn seed_runs(root: &Path, agent: &str, count: usize, log_bytes: usize) {
    for index in 0..count {
        let run_id = format!("run-{}-{index:04}-1", 1_700_000_000_000u128 + index as u128);
        let dir = root.join("state").join(agent).join(&run_id);
        fs::create_dir_all(&dir).expect("the run directory is created");
        let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
        transcript
            .append(&Record::run_started(&run_id, agent, 42, "/w"))
            .expect("a record");
        transcript
            .append(&Record::run_finished(&run_id, RunOutcome::Succeeded, Some(0), 120, None))
            .expect("a record");
        if log_bytes > 0 {
            fs::write(dir.join("run.log"), vec![b'x'; log_bytes]).expect("the log is written");
        }
    }
}

#[test]
fn the_runs_list_and_the_footer_count_say_when_they_stop_at_fifty() {
    // L-1 / L-5: the record list and the footer count both stopped at fifty with no
    // word, presenting fifty as the whole history. The list now declares the older
    // records, and the footer says `+` when it cannot count them all.
    let root = scratch("cap-declared");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    seed_runs(&root, "scribe", 60, 0);
    app.reload();

    assert_eq!(app.recent.len(), 50, "the list is capped at fifty");
    assert_eq!(app.recent_total, 60, "all sixty records are measured");
    assert!(app.finished_count_is_capped(), "the count cannot see them all");

    app.show(Screen::Runs);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("This list stops at the newest 50 of the 60 run records on disk"),
        "the cap is not declared: {screen}"
    );
    assert!(
        screen.contains("10 older") && screen.contains("not listed"),
        "the older records are not named: {screen}"
    );
    assert!(
        screen.contains("50+ finished"),
        "the footer count presents the cap as exact: {screen}"
    );
}

#[test]
fn one_fact_gets_one_number_on_the_runs_screen() {
    // F4 / H-4: the footprint said `120 run records on disk` and the cap clause said
    // `of 110 on disk`, and `60 older records are not listed` omitted the unlisted
    // agent's records. Every total is now named for what it counts, and the size says
    // it is file bytes.
    let root = scratch("one-number");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    // Sixty records of a configured agent and ten of one the configuration dropped.
    seed_runs(&root, "scribe", 60, 0);
    seed_runs(&root, "ghost", 10, 0);
    app.reload();
    assert_eq!(app.footprint.records, 70, "every record is measured");
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("70 run records on disk")
            && screen.contains("of file bytes in total"),
        "the footprint is not one named quantity: {screen}"
    );
    // R-5: the list now covers every record on disk; the only difference the cap
    // clause declares is the cap itself, and the unconfigured records are listed and
    // marked rather than merely counted.
    assert!(
        screen.contains("run records on disk"),
        "the total on disk is not named: {screen}"
    );
    assert!(
        screen.contains("20 older records") && screen.contains("not listed"),
        "the cap difference is not named: {screen}"
    );
    // W36-3: the ten ghost records are all older than the newest fifty, so the list
    // carries none of them. The note must not point at `*` rows that are not there;
    // it says the count and the marker's presence separately.
    assert!(
        screen.contains("10 records belong")
            && screen.contains("none is among the newest")
            && screen.contains("carries the `*` marker here"),
        "the unconfigured records' marker claim does not match the list: {screen}"
    );
}

#[test]
fn the_credentials_present_list_is_reachable_at_the_floor() {
    // W36-1, runnable on the unfixed tree too: the joined `present` list was one
    // logical line; at 72×18 it was taller than the panel body, so `fit_panel_lines`
    // dropped it and no `[`/`]` offset could bring it back. Every name must be drawn
    // at some offset.
    let norm = |text: &str| -> String { text.chars().filter(|c| c.is_alphanumeric()).collect() };
    let root = scratch("creds-present-floor");
    let mut config = String::from(
        "[forgejo]\nurl = \"https://forgejo.example.org\"\nrole = \"member\"\n",
    );
    for index in 0..31 {
        config.push_str(&format!(
            "[[agents]]\nname = \"agent-{index:02}\"\nmodel = \"m\"\n\
             endpoint = \"https://e/v1\"\n\
             instruction = \"agents/agent-{index:02}/instruction.md\"\nconcurrency = 1\n"
        ));
    }
    let mut app = app_with_config(&root, &config);
    app.show_acknowledgment = false;
    let mut text = String::from("[model_keys]\n");
    for index in 0..31 {
        text.push_str(&format!("\"agent-{index:02}\" = \"gw-fixture-key-{index:02}\"\n"));
    }
    gewu_multiverse::mode::write_private(root.join("credentials.toml"), text.as_bytes())
        .expect("the credentials file is written");
    app.reload();
    app.show(Screen::Credentials);
    let total = gewu_multiverse::ui::panel_total_lines(&app);
    let mut drawn = String::new();
    for _ in 0..=total {
        drawn.push_str(&norm(&render_at(72, 18, &app)));
        app.scroll_panel(1);
    }
    for index in 0..31 {
        let needle = norm(&format!("model key for `agent-{index:02}`"));
        assert!(
            drawn.contains(&needle),
            "the present name for agent-{index:02} is unreachable at 72×18"
        );
    }
}

#[test]
fn the_unconfigured_sentence_separates_the_count_from_the_listed_marker() {
    // W36-3: when only some of the unconfigured records are inside the newest fifty,
    // the count and the marked subset are stated separately, so the list above never
    // contradicts the sentence.
    let root = scratch("unconfigured-partial");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let summary = |agent: &str, index: u64| RunSummary {
        agent: agent.to_owned(),
        run_id: new_run_id(index),
        outcome: Some(RunOutcome::Succeeded),
        reason: None,
        duration_ms: Some(100),
        started_at: Some("2026-10-15T00:00:00Z".to_owned()),
        log: PathBuf::new(),
        dir: PathBuf::new(),
    };
    app.recent = vec![summary("ghost", 0), summary("scribe", 0), summary("ghost", 1)];
    app.recent_total = 10;
    app.footprint.records = 10;
    app.footprint.unconfigured_records = 5;
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(screen.contains("5 records belong"), "{screen}");
    assert!(
        normalised(&screen).contains(&normalised(
            "the 2 among the newest 3 listed above are marked `*`"
        )),
        "the marked subset is not named: {screen}"
    );
}

#[test]
fn the_address_prompt_says_what_the_address_is_for() {
    // W37-3: the second clause ("this machine is the only one it names") was opaque.
    // The prompt now says the address applies to every agent and what checks against
    // it, which is what the map asks for.
    let root = scratch("url-prompt");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    app.request_url_edit();
    let screen = render_at(100, 40, &app);
    assert!(
        normalised(&screen).contains(&normalised("every agent on this machine uses")),
        "{screen}"
    );
    assert!(
        normalised(&screen).contains(&normalised("the role check runs against it")),
        "the prompt does not say what checks against the address: {screen}"
    );
    assert!(
        !screen.contains("the only one it names"),
        "the opaque second clause is still there: {screen}"
    );
}

#[test]
fn the_run_records_footprint_and_the_cleanup_expectation_are_on_the_runs_screen() {
    // L-2: records accumulated with no footprint and no statement of what a person is
    // expected to do about them. The Runs screen now shows the count, the room and the
    // expectation (the product never deletes a record; the person removes a run's
    // directory to free its space).
    let root = scratch("footprint");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    seed_runs(&root, "scribe", 3, 2048);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("3 run records on disk"),
        "the footprint count is missing: {screen}"
    );
    assert!(
        screen.contains("KiB") || screen.contains("B of file bytes"),
        "the footprint has no room figure: {screen}"
    );
    assert!(
        screen.contains("it never deletes one by itself"),
        "the cleanup expectation is not stated: {screen}"
    );
}

#[test]
fn a_removed_agents_records_are_listed_and_marked() {
    // L-4 / R-5: removing an agent used to drop all its records from the list while
    // the footprint counted them, so their transcript and log could not be opened in
    // the product until the agent was re-added. The records are now listed, marked
    // `*`, and the count and the rows agree.
    let root = scratch("removed-records");
    let mut app = app_with_config(&root, EMPTY_CONFIG);
    app.show_acknowledgment = false;
    seed_runs(&root, "ghost", 4, 0);
    app.reload();
    assert_eq!(app.footprint.records, 4, "every record is measured");
    assert_eq!(
        app.footprint.unconfigured_records, 4,
        "the records of the removed agent are the unconfigured ones"
    );
    assert_eq!(
        app.recent.len(),
        4,
        "the removed agent's records are listed, not only counted (R-5)"
    );
    app.show(Screen::Runs);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("4 run records on disk"),
        "the records are not counted: {screen}"
    );
    assert!(
        screen.contains("no longer lists") && screen.contains("are marked `*`"),
        "the unconfigured records are not named as listed: {screen}"
    );
    assert!(screen.contains('*'), "the rows carry no marker: {screen}");
}

#[test]
fn the_agent_screen_names_the_size_of_the_log_and_the_transcript() {
    // L-3: the files that grow with every run had no size anywhere; only the
    // instruction had the byte arithmetic. The Agent screen names both, with the size.
    let root = scratch("growing-files");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    seed_runs(&root, "scribe", 1, 4096);
    app.reload();
    app.show(Screen::AgentConfig);
    let screen = render_at(100, 50, &app);
    assert!(screen.contains("newest log"), "the log field is gone: {screen}");
    assert!(screen.contains("4.0 KiB"), "the log's size is not shown: {screen}");
    assert!(
        screen.contains("transcript"),
        "the transcript field is not drawn: {screen}"
    );
}

#[test]
fn the_forgejo_identity_sentence_does_not_stop_mid_thought() {
    // C-2: the one line that answers "whose identity?" ended at `the token is from v`,
    // at every width. It is a finished sentence now, and it names the key that does it.
    let root = scratch("identity-sentence");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render_at(200, 30, &app);
    assert!(
        screen.contains("v supplies the token"),
        "the identity sentence is not complete: {screen}"
    );
    assert!(
        !screen.contains("from v"),
        "the broken clause is still there: {screen}"
    );
}

#[test]
fn a_used_configuration_says_when_and_under_which_version_the_risk_was_acknowledged() {
    // C-3: a second person on a directory someone else acknowledged had no record of
    // it anywhere on screen, and no way to read the terms. The Agents note area names
    // the record, the date and the version, and `R` shows the terms read-only.
    let root = scratch("ack-visible");
    let mut app = app(&root);
    // The record a predecessor wrote.
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.show(Screen::Agents);
    let screen = render(&app);
    assert!(
        screen.contains("the risk was acknowledged on"),
        "the record is not on the screen: {screen}"
    );
    assert!(
        screen.contains("press R to") && screen.contains("read the terms again"),
        "there is no way to read the terms again: {screen}"
    );
    assert!(
        screen.contains(env!("CARGO_PKG_VERSION")),
        "the version that wrote the record is not named: {screen}"
    );
    // F3: the version now running is named beside the record's own version, so the
    // comparison a second person came for is on the screen.
    assert!(
        screen.contains("this build is"),
        "the running version is not labelled: {screen}"
    );

    // `R` shows the terms without writing anything.
    let record_before = fs::read(root.join("risk-acknowledgment.toml")).expect("the record");
    app.on_key(KeyEvent::from(KeyCode::Char('R')));
    assert!(app.show_acknowledgment, "the terms are not shown");
    assert!(app.ack_review, "the review is not read-only");
    let review = render(&app);
    assert!(
        review.contains("the record is unchanged"),
        "the review does not say it writes nothing: {review}"
    );
    app.on_key(KeyEvent::from(KeyCode::Esc));
    let record_after = fs::read(root.join("risk-acknowledgment.toml")).expect("the record");
    assert_eq!(record_before, record_after, "the review changed the record");
}

#[test]
fn a_leftover_secret_names_the_age_of_the_credentials_file() {
    // C-4: the Credentials screen marked the leftover but gave no age, so a person
    // could not tell a tidy-up from a loss. The file's own modification time is named.
    let root = scratch("leftover-age");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let credentials = root.join("credentials.toml");
    fs::write(
        &credentials,
        "[model_keys]\n\"ghost\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
    // Three days old, so the sentence has an age to name.
    let three_days_ago = SystemTime::now() - std::time::Duration::from_secs(3 * 86_400 + 120);
    let file = fs::File::options()
        .write(true)
        .open(&credentials)
        .expect("the credentials file opens");
    file.set_modified(three_days_ago)
        .expect("the modification time is set");
    app.reload();
    app.show(Screen::Credentials);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("last written 3 days ago"),
        "the leftover has no age: {screen}"
    );
}

#[test]
fn the_editor_confirmation_names_the_file_and_agrees_with_its_subject() {
    // F5: the overlay said `press Y … or N to leave without saving`, but nano with an
    // unmodified buffer leaves at `^X` at once, so the person's `N` reached the
    // product and opened the add-agent form. The advice must be true in both cases.
    // F6: `the declared steps of `scribe` opens in` did not agree with its subject.
    let root = scratch("editor-confirm");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.editor_configured = false;
    app.show(Screen::Instructions);
    app.on_key(KeyEvent::from(KeyCode::Char('t')));
    assert!(app.editor_confirm.is_some(), "the confirmation is up");
    let screen = render(&app);
    assert!(
        screen.contains("the declared steps of `scribe` open in"),
        "{screen}"
    );
    assert!(screen.contains("if it asks"), "{screen}");
    assert!(
        !screen.contains("N to leave without saving"),
        "the advice still promises a prompt that may never come: {screen}"
    );
}

#[test]
fn a_set_editor_is_named_before_it_opens() {
    // N-1: with `$EDITOR` set the product went straight into the editor, so the
    // program was named only on return. It is now named by the same confirmation the
    // unset case uses, before the terminal is handed over.
    let root = scratch("editor-configured-confirm");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.editor_configured = true;
    app.show(Screen::Instructions);
    app.on_key(KeyEvent::from(KeyCode::Char('e')));
    assert!(
        app.editor_confirm.is_some(),
        "the configured editor takes the screen with no confirmation"
    );
    assert!(
        app.take_action().is_none(),
        "the hand-off must wait for the confirmation"
    );
    let screen = render(&app);
    assert!(
        screen.contains("`$EDITOR` is set, so the instruction of `scribe` opens in"),
        "the program is not named before it opens: {screen}"
    );
    assert!(screen.contains("enter open it"), "{screen}");
    assert!(screen.contains("esc cancel"), "{screen}");
}

#[test]
fn the_acknowledgment_modal_is_whole_or_declares_itself_at_the_floor() {
    // F-1 (severity 3): at 72×18 the modal drew half a wrapped line and said nothing.
    // It now follows the same rule as every bordered panel: whole logical lines, and
    // `more below; enlarge the window` in the title when any was dropped, with the
    // answer line kept as the last drawn row. This is the guard at the floor.
    let root = scratch("ack-floor");
    let mut app = app(&root);
    assert!(app.show_acknowledgment, "the first-run modal is up");
    // At the advertised minimum (80×24) the modal is whole and declares nothing.
    let wide = render_at(80, 24, &app);
    assert!(
        !wide.contains("Before the first agent runs — more below"),
        "the modal declares an overflow it does not have at 80×24: {wide}"
    );
    assert!(wide.contains("y — I understand, record it once"), "{wide}");
    assert!(wide.contains("The record will be written to"), "{wide}");
    // At the 72×18 floor the prose is dropped whole and the title says so; the answer
    // line survives as the last drawn row.
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains("Before the first agent runs — more below; enlarge the window"),
        "the modal drops content in silence at the floor: {floor}"
    );
    assert!(
        floor.contains("y — I understand, record it once"),
        "the answer line is not drawn at the floor: {floor}"
    );
    // The read-only review modal declares and keeps its own close line at the floor.
    app.ack_review = true;
    let review = render_at(72, 18, &app);
    assert!(
        review.contains("Before the first agent runs — more below"),
        "the review modal drops content in silence at the floor: {review}"
    );
    assert!(
        review.contains("any key closes this; the record is unchanged"),
        "the review modal has no visible way out at the floor: {review}"
    );
    // V-2: the footer agrees with the body — `esc` is not the only key that closes it,
    // so the surface and the footer no longer make two different claims.
    assert!(
        review.contains("any key closes this overlay; the record is unchanged"),
        "the review modal's footer disagrees with its own close line: {review}"
    );
}

#[test]
fn a_wrapped_identity_never_glues_the_login_to_the_email() {
    // N-2: at the width where the identity wraps at its own space, the next row began
    // with the email and the login read as glued to it. The separator is a no-break
    // space now, so the pair wraps whole.
    let root = scratch("wrapped-identity");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let paths = app.selected_paths().expect("paths");
    let workspace = paths.workspace();
    fs::create_dir_all(workspace).expect("the workspace exists");
    let git = |args: &[&str]| {
        let status = process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "someone-else"]);
    git(&["config", "user.email", "someone@else.invalid"]);
    git(&["commit", "--allow-empty", "-m", "one"]);
    app.reload();
    app.show(Screen::AgentConfig);
    // The workspace note is read once a second while its screen is on show.
    app.tick();
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("someone-else\u{a0}<someone@else.invalid>"),
        "the identity pair is not protected from the wrap: {screen}"
    );
    assert!(
        !screen.contains("someone-else\n<someone@else.invalid>"),
        "the login and the email were split at their own space: {screen}"
    );
}

#[test]
fn an_endpoint_test_from_another_machine_is_not_drawn_as_this_machines_result() {
    // W32-2: a test result belongs to a machine and a time. A record copied from
    // another machine must not be drawn as if this machine's endpoint had answered.
    use gewu_multiverse::app::{EndpointFailure, EndpointTest};
    let root = scratch("endpoint-machine");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::AgentConfig);
    let base = EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: "https://model.example.org/v1".to_owned(),
        model: "example-model".to_owned(),
        result: Ok("the endpoint answered: ok".to_owned()),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: Some("another-host/00000000000000000000000000000000".to_owned()),
    };
    app.endpoint_test = Some(base.clone());
    let screen = render_at(100, 40, &app);
    assert!(screen.contains("tested just now"), "{screen}");
    assert!(
        normalised(&screen).contains(&normalised("tested on another machine")),
        "a foreign machine's test is drawn as this machine's: {screen}"
    );

    // A record that names no machine says so, rather than claiming either way.
    app.endpoint_test = Some(EndpointTest {
        machine: None,
        ..base.clone()
    });
    let screen = render_at(100, 40, &app);
    assert!(
        normalised(&screen).contains(&normalised("does not name the machine")),
        "{screen}"
    );

    // This machine's own mark gains no clause.
    app.endpoint_test = Some(EndpointTest {
        machine: gewu_multiverse::machine::mark(),
        ..base
    });
    let screen = render_at(100, 40, &app);
    assert!(
        !screen.contains("tested on another machine")
            && !screen.contains("does not name the machine"),
        "{screen}"
    );
    // The class and the cause of a failure are still drawn beside the age.
    app.endpoint_test = Some(EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: "https://model.example.org/v1".to_owned(),
        model: "example-model".to_owned(),
        result: Err(EndpointFailure {
            class: "transport".to_owned(),
            message: "the endpoint refused the connection".to_owned(),
        }),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: gewu_multiverse::machine::mark(),
    });
    let screen = render_at(100, 40, &app);
    assert!(screen.contains("transport"), "{screen}");
    assert!(screen.contains("refused the connection"), "{screen}");
}

#[test]
fn a_settled_identity_and_its_last_failure_are_one_verdict() {
    // W32-5: the panel drew a settled identity sentence and, below it, the last
    // identity failure with no word joining them, so a reader could not tell whether
    // the identity was usable now. The failure is now appended to the identity line,
    // and the tail no longer repeats it.
    use gewu_multiverse::app::{ForgejoFailure, IdentityReport};
    use gewu_multiverse::config::Role;
    use gewu_multiverse::forgejo::{Permissions, Repository, RoleCheck, Viewer};
    use gewu_multiverse::identity::IdentityMode;
    let root = scratch("identity-connected");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    let credentials = app.credentials_path.clone();
    fs::write(
        &credentials,
        "[forgejo_logins]\nscribe = \"onboarder\"\n\n[forgejo_emails]\nscribe = \"onboarder@example.invalid\"\n",
    )
    .expect("the credentials file");
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).expect("0600");
    app.role_check = Some(Ok(RoleCheck {
        url: "http://127.0.0.1:39634".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "onboarder".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: vec![Repository {
            full_name: "onboarder/notes".to_owned(),
            namespace: "onboarder".to_owned(),
            name: "notes".to_owned(),
            private: false,
            default_branch: Some("main".to_owned()),
            html_url: None,
            permissions: Permissions {
                admin: false,
                push: true,
                pull: true,
            },
        }],
        private_visible: 0,
    }));
    app.identity_report = Some(IdentityReport {
        agent: "scribe".to_owned(),
        result: Err(ForgejoFailure {
            class: "bad_token".to_owned(),
            message: "the Forgejo refused the token (HTTP 401)".to_owned(),
        }),
    });
    app.show(Screen::Forgejo);
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("onboarder\u{a0}<onboarder@example.invalid>"),
        "the settled identity left the panel: {screen}"
    );
    assert!(
        screen.contains("the last check failed (bad_token)"),
        "the failure is not joined to the identity: {screen}"
    );
    // The tail copy of a failed report is gone: one verdict, not two.
    assert!(
        !screen.contains("identity for `scribe`: failed"),
        "the failure is still drawn twice: {screen}"
    );
    let _ = IdentityMode::Supplied;
}

#[test]
fn the_first_run_modal_leaves_no_fragment_of_the_note_area() {
    // F-1: at 100x40 the first screen showed `writes both files for you.` below the
    // modal — the tail of a sentence whose head the modal hid. While the modal owns
    // the screen, the note area is not drawn.
    let root = scratch("first-run-fragment");
    let mut app = app(&root);
    app.show_acknowledgment = true;
    let screen = render_at(100, 40, &app);
    assert!(
        !screen.contains("writes both files for you"),
        "a dangling fragment of the note area is on the first screen: {screen}"
    );
    assert!(
        screen.contains("Before the first agent runs"),
        "the modal itself is missing: {screen}"
    );
    // Once the modal closes, the note area returns whole.
    app.show_acknowledgment = false;
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("concurrency is how many model requests"),
        "the note area did not return: {screen}"
    );
}

/// The note-area guard: every overlay that owns the screen owns the note area behind
/// it.
///
/// R-3 / S-1, F-1's third appearance. The first-run modal was fixed on its own; the
/// class survived on the add-agent form. This test enumerates every overlay in the
/// code — not the screens a walker happened to visit — draws each over the Agents and
/// Runs screens at the two sizes the ORDER names and the taller window the walker
/// found the fragment at (100×40), and fails if any fragment of a note-area sentence
/// survives behind the overlay. The two note areas use the same predicate, so
/// exercising both guards the class, not one screen. Closing the overlays restores
/// the note areas whole.
#[test]
fn every_overlay_owns_the_note_area_behind_it() {
    // 72×18 and 80×24 are the advertised sizes; 100×40 and 120×40 are where a short
    // overlay over a tall window left the tail of a wrapped sentence showing.
    const SIZES: [(u16, u16); 4] = [(72, 18), (80, 24), (100, 40), (120, 40)];
    type Opener = Box<dyn Fn(&mut App)>;

    // An empty hub: the note area is the one wrapped sentence whose tail
    // (`writes both files for you.`) was the R-3/S-1 fragment.
    let empty = scratch("overlay-note-fragment-empty");
    gewu_multiverse::mode::write_private(
        empty.join("credentials.toml"),
        b"[model_keys]\n\"ghost\" = \"gw-fixture-ghost\"\n",
    )
    .expect("the credentials file is written");
    let empty_openers: Vec<(&str, Opener)> = vec![
        (
            "first-run acknowledgment",
            Box::new(|app: &mut App| app.show_acknowledgment = true),
        ),
        (
            "add-agent form",
            Box::new(|app: &mut App| app.request_new_agent()),
        ),
        (
            "Forgejo address prompt",
            Box::new(|app: &mut App| app.request_url_edit()),
        ),
        (
            "leftover-secret delete",
            Box::new(|app: &mut App| app.request_forget_secrets()),
        ),
        ("key help", Box::new(|app: &mut App| app.toggle_help())),
    ];
    for (label, open) in &empty_openers {
        for (width, height) in SIZES {
            let mut app = app_with_config(&empty, EMPTY_CONFIG);
            app.show_acknowledgment = false;
            open(&mut app);
            assert!(
                app.overlay_owns_screen(),
                "{label} did not open, so the guard proves nothing"
            );
            let screen = render_at(width, height, &app);
            assert!(
                !screen.contains("writes both files for you"),
                "{label} at {width}x{height} left the agent note-area tail behind it:\n{screen}"
            );
            app.show(Screen::Runs);
            let runs = render_at(width, height, &app);
            assert!(
                !runs.contains("Run records are read from"),
                "{label} at {width}x{height} left the run note area behind it:\n{runs}"
            );
        }
    }

    // A configured hub: the overlays that need an agent, over the longer note area.
    // Every distinctive note line is gone while an overlay is up.
    let full = scratch("overlay-note-fragment-full");
    let full_openers: Vec<(&str, Opener)> = vec![
        (
            "add-agent form",
            Box::new(|app: &mut App| app.request_new_agent()),
        ),
        (
            "secret prompt",
            Box::new(|app: &mut App| app.request_secret(SecretTarget::ModelKey)),
        ),
        (
            "remove confirmation",
            Box::new(|app: &mut App| app.request_remove_agent()),
        ),
        (
            "editor confirmation",
            Box::new(|app: &mut App| {
                app.editor_configured = false;
                app.on_key(KeyEvent::from(KeyCode::Char('e')));
            }),
        ),
        ("key help", Box::new(|app: &mut App| app.toggle_help())),
    ];
    for (label, open) in &full_openers {
        for (width, height) in SIZES {
            let mut app = app(&full);
            app.show_acknowledgment = false;
            open(&mut app);
            assert!(
                app.overlay_owns_screen(),
                "{label} did not open, so the guard proves nothing"
            );
            let screen = render_at(width, height, &app);
            for fragment in [
                "is not acknowledged",
                "concurrency is how many model requests",
                "not a sandbox",
            ] {
                assert!(
                    !screen.contains(fragment),
                    "{label} at {width}x{height} left the note-area fragment `{fragment}` behind it:\n{screen}"
                );
            }
        }
    }

    // The note areas return whole the moment no overlay owns the screen.
    let mut empty_app = app_with_config(&empty, EMPTY_CONFIG);
    empty_app.show_acknowledgment = false;
    assert!(
        render_at(100, 40, &empty_app).contains("writes both files for you"),
        "the empty-hub note area did not return"
    );
    let mut app = app(&full);
    app.show_acknowledgment = false;
    assert!(
        render_at(100, 40, &app).contains("concurrency is how many model requests"),
        "the agent note area did not return"
    );
}

/// The in-flight guard for S-2: a probe that has started says so, draws no previous
/// result as current, and its finished result is stamped.
#[test]
fn the_endpoint_test_says_it_is_in_flight_and_stamps_the_result() {
    use gewu_multiverse::test_stub::{Stub, StubReply};

    let completion = r#"{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
    let mut reply = StubReply::json(completion);
    reply.delay = std::time::Duration::from_millis(1500);
    let stub = Stub::start(vec![reply]);
    let root = scratch("endpoint-in-flight");
    fs::write(
        root.join("config.toml"),
        CONFIG.replace("https://model.example.org/v1", &stub.url()),
    )
    .expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("endpoint-in-flight"),
    );
    app.show_acknowledgment = false;
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
    app.selected = 0;
    app.show(Screen::AgentConfig);
    app.on_key(KeyEvent::from(KeyCode::Char('p')));
    // No frame is drawn between the key and this render, so the probe is in flight.
    let during = render_at(100, 40, &app);
    assert!(
        during.contains("testing — started"),
        "the endpoint test draws no in-flight state:\n{during}"
    );
    assert!(
        app.status.contains("testing the endpoint of `scribe`"),
        "the footer does not say a test is running: {}",
        app.status
    );
    assert!(
        !during.contains("not tested yet") && !during.contains("the endpoint answered"),
        "a previous or absent result is drawn while the probe runs:\n{during}"
    );
    app.wait_for_probe();
    let after = render_at(100, 40, &app);
    assert!(
        after.contains("the endpoint answered") && after.contains("tested just now"),
        "the finished result is not shown and stamped:\n{after}"
    );
}
