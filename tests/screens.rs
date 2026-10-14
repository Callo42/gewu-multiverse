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
instruction = "a1.md"
concurrency = 1

[[agents]]
name = "a2"
model = "m"
endpoint = "https://m.example.org"
instruction = "a2.md"
concurrency = 1

[[agents]]
name = "a3"
model = "m"
endpoint = "https://m.example.org"
instruction = "a3.md"
concurrency = 1

[[agents]]
name = "a4"
model = "m"
endpoint = "https://m.example.org"
instruction = "a4.md"
concurrency = 1

[[agents]]
name = "a5"
model = "m"
endpoint = "https://m.example.org"
instruction = "a5.md"
concurrency = 1

[[agents]]
name = "a6"
model = "m"
endpoint = "https://m.example.org"
instruction = "a6.md"
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
    )
    .expect("the identity is recorded");
    app.reload();
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("gewu-scribe (managed)"), "{screen}");
    assert!(screen.contains("act as this account"), "{screen}");
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
    assert!(with_files.contains("are left on disk"), "{with_files}");
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
    assert!(first.contains("of 16"), "the range is missing: {first}");
    // Scroll to the end; the last line must be reachable.
    for _ in 0..20 {
        app.on_key(KeyEvent::from(KeyCode::Down));
    }
    let last = render_at(80, 24, &app);
    assert!(
        last.contains("esc or enter close it"),
        "the last help line is unreachable: {last}"
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
fn the_identity_line_names_the_account_i_would_create() {
    // D-4: `i` acts on a remote system; name the account before the keypress.
    let root = scratch("identity-preview");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("not settled"), "{screen}");
    assert!(screen.contains("`gewu-scribe`"), "{screen}");
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

    // The Agents note area has no border to carry a title, so its own last row says so.
    app.show(Screen::Agents);
    let agents = render_at(80, 24, &app);
    assert!(
        agents.contains("… more below; enlarge the window"),
        "{agents}"
    );
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
    assert!(last.contains("enlarge the window"), "{last}");
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
instruction = "instructions/scribe.md"
concurrency = 1
"#;
    let root = scratch("endpoint-failure-80");
    let mut app = app_with_config(&root, CONFIG);
    app.show_acknowledgment = false;
    app.request_secret(SecretTarget::ModelKey);
    app.on_paste("sk-not-a-real-credential");
    app.on_key(KeyEvent::from(KeyCode::Enter));
    app.request_endpoint_test();
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
    app.show(Screen::AgentConfig);
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
    // declare, with the repository range still present.
    let short = render_at(80, 18, &app);
    let title_line = short
        .lines()
        .find(|line| line.contains("┌ Forgejo"))
        .unwrap_or_default();
    assert!(
        title_line.contains("repositories 1–1 of 7"),
        "the repository range left the title: {title_line:?}"
    );
    assert!(
        title_line.contains("more below; enlarge the window"),
        "the declaration is clipped or missing: {title_line:?}"
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
    assert!(app
        .present_secret_labels()
        .iter()
        .any(|label| label.contains("model key for `scribe`")));

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
