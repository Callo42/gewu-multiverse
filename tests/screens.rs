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
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, new_run_id};
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
instruction = "instructions/scribe.md"
concurrency = 2

[[agents]]
name = "checker"
model = "other-model"
endpoint = "https://model.example.org/v1"
instruction = "instructions/checker.md"
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
    assert!(instructions.contains("turn(s)"), "{instructions}");

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
    assert!(runs.contains("No run has been recorded yet"), "{runs}");
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
        }),
    });
    app.show(Screen::Forgejo);
    let screen = render(&app);
    show("Forgejo (checked)", &app);
    assert!(screen.contains("administrative view"), "{screen}");
    assert!(screen.contains("root/private"), "{screen}");
    assert!(screen.contains("private"), "{screen}");
    assert!(screen.contains("root sees 2 repository(ies)"), "{screen}");
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
