//! Guards for the twenty-seventh fix round's seven classes, driven through the
//! product's own `App`, its records, its run process and the frames it draws.
//!
//! 1. the model's final reply is readable in the run view and complete in the log;
//! 2. transport and declared-step failures name the endpoint, the command, the
//!    working directory and an output excerpt;
//! 3. primary actions and the real scroll keys survive 72×18 and 80×24, the
//!    Instruction screen previews its document and steps, and a state cell is whole;
//! 4. one colour scheme with bright-red errors, bright-yellow warnings, default
//!    information and the marked inverse selection;
//! 5. the role label follows the supplied token, an administrator token is not asked
//!    for twice, the dead `C` is not offered without one, and the identity sentence
//!    says what the account does;
//! 6. no host name or machine id is printed on a screen;
//! 7. the README says there is no scheduler, and the quit warning/receipt names the
//!    waiting agents.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, EndpointTest, Screen};
use gewu_multiverse::config::Role;
use gewu_multiverse::forgejo::{RoleCheck, Viewer};
use gewu_multiverse::model::{self, ModelConfig};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, RunPlan};
use gewu_multiverse::supervisor::Supervisor;
use gewu_multiverse::systemd::{Mode, Systemctl};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{ModelUsage, Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

const ONE_AGENT: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 2
"#;

const EMPTY_CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-r27-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn quiet_runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("runner"),
    );
    app.show_acknowledgment = false;
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
    app
}

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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

/// A finished run written as the product writes it: an opening record, one model
/// reply and a terminal record.
fn write_run(root: &Path, content: &str, reason: Option<&str>) -> PathBuf {
    let layout = Layout::rooted_at(root.to_path_buf());
    let run_id = "run-1700000000000-0001";
    let run_dir = layout.state_root().join("scribe").join(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(run_id, "scribe", 4321, "/w"))
        .expect("a run starts");
    transcript
        .append(&Record::model_reply(
            run_id,
            1,
            200,
            Some(content.to_owned()),
            Vec::new(),
            Some("stop".to_owned()),
            Some(ModelUsage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            }),
            "{}",
        ))
        .expect("a reply is recorded");
    transcript
        .append(&Record::run_finished(
            run_id,
            if reason.is_some() {
                RunOutcome::Failed
            } else {
                RunOutcome::Succeeded
            },
            Some(if reason.is_some() { 1 } else { 0 }),
            1200,
            reason.map(str::to_owned),
        ))
        .expect("the run ends");
    fs::write(run_dir.join(gewu_multiverse::run::LOG_FILE), "a log\n").expect("a log is written");
    run_dir
}

/// A ready agent: instruction, skills directory and model key.
fn ready_agent(root: &Path) -> gewu_multiverse::paths::AgentPaths {
    let layout = Layout::rooted_at(root.to_path_buf());
    let paths = layout
        .ensure_agent("scribe")
        .expect("the agent is prepared");
    fs::write(paths.instruction(), "do the thing\n").expect("the instruction is written");
    fs::create_dir_all(paths.skills_dir()).expect("the skills directory is created");
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
    paths
}

fn plan(paths: &gewu_multiverse::paths::AgentPaths, run_id: &str, endpoint: &str) -> RunPlan {
    RunPlan {
        run_id: run_id.to_owned(),
        agent: "scribe".to_owned(),
        workspace: paths.workspace().to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: endpoint.to_owned(),
        model: "example-model".to_owned(),
        credentials: paths.root().join("credentials.toml"),
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 10,
    }
}

fn completion(content: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
    })
    .to_string()
}

fn cell_style_after(app: &App, width: u16, height: u16, needle: &str) -> (Color, Color, Modifier) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, app))
        .expect("a frame is drawn");
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            row.push_str(buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" "));
        }
        if let Some(at) = row.find(needle) {
            let cell = buffer
                .cell((at as u16, y))
                .expect("the needle's cell is in the buffer");
            return (cell.fg, cell.bg, cell.modifier);
        }
    }
    panic!(
        "the needle {needle:?} is not drawn:\n{}",
        render_at(width, height, app)
    );
}

// 1. Readable output -------------------------------------------------------

#[test]
fn the_model_final_reply_is_readable_in_the_run_view_and_complete_in_the_log() {
    let root = scratch("final-reply-view");
    let mut app = app_with(&root, ONE_AGENT);
    let reply = "## Release notes draft\n\n### Added\n- one item\n\n### Fixed\n- another item\n\nTHE-END-OF-THE-REPLY";
    write_run(&root, reply, None);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("the model's final reply (readable, copyable)"),
        "{screen}"
    );
    assert!(
        screen.contains("THE-END-OF-THE-REPLY"),
        "the reply tail is not drawn:\n{screen}"
    );
    assert!(screen.contains("raw transcript (JSONL)"), "{screen}");

    // The log of a real run carries the same text, not an 80-byte excerpt.
    let log_root = scratch("final-reply-log");
    let paths = ready_agent(&log_root);
    let stub = Stub::start(vec![StubReply::json(completion(reply))]);
    let run_id = "run-1700000000000-0001";
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    plan(&paths, run_id, &stub.url())
        .write(&run_dir)
        .expect("the plan is written");
    let end = gewu_multiverse::agent_run::execute(&run_dir);
    assert_eq!(end.exit_code, 0, "{:?}", end.reason);
    let log = fs::read_to_string(run_dir.join(gewu_multiverse::run::LOG_FILE))
        .expect("the log is readable");
    assert!(
        log.contains("THE-END-OF-THE-REPLY"),
        "the log truncates the final reply:\n{log}"
    );
    assert!(
        !log.contains("80 of"),
        "the log still says how little it showed:\n{log}"
    );
}

// 2. Honest failures -------------------------------------------------------

#[test]
fn a_transport_failure_names_the_endpoint_and_the_endpoint_test_names_its_facts() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    drop(listener);
    let config = ModelConfig {
        endpoint: endpoint.clone(),
        model: "example-model".to_owned(),
        api_key: gewu_multiverse::credentials::Secret::new("gw-fixture-key"),
        timeout: Duration::from_millis(500),
    };
    let error = model::probe(&config)
        .expect_err("a closed port refuses")
        .to_string();
    assert!(
        error.contains(&endpoint),
        "the endpoint is not named: {error}"
    );
    assert!(error.contains("refused"), "{error}");

    let test = EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: endpoint.clone(),
        model: "example-model".to_owned(),
        result: Ok("the endpoint answered: ok".to_owned()),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: None,
    };
    let line = test.describe();
    assert!(line.contains(&endpoint), "{line}");
    assert!(line.contains("example-model"), "{line}");
    assert!(line.contains("tested just now"), "{line}");
}

#[test]
fn a_failed_declared_step_names_its_command_directory_and_output() {
    let root = scratch("step-failure");
    let paths = ready_agent(&root);
    let run_id = "run-1700000000000-0001";
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut plan = plan(&paths, run_id, "http://127.0.0.1:9/v1/chat/completions");
    plan.steps = vec!["printf 'boom from the step\\n' >&2; exit 7".to_owned()];
    plan.write(&run_dir).expect("the plan is written");
    let end = gewu_multiverse::agent_run::execute(&run_dir);
    let reason = end.reason.expect("the step ends the run");
    assert!(reason.contains("printf 'boom from the step"), "{reason}");
    assert!(
        reason.contains(&paths.workspace().display().to_string()),
        "{reason}"
    );
    assert!(reason.contains("boom from the step"), "{reason}");
}

// 3. Nothing primary below the fold ---------------------------------------

#[test]
fn the_primary_actions_and_the_real_scroll_keys_are_visible_at_the_floor() {
    let root = scratch("floor-actions");
    let app = app_with(&root, EMPTY_CONFIG);
    let empty = render_at(72, 18, &app);
    assert!(
        empty.contains("press n"),
        "the add action is below the fold:\n{empty}"
    );
    assert!(
        empty.contains("more below — press ]"),
        "the scroll key is not named:\n{empty}"
    );

    let root = scratch("floor-forgejo");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Forgejo);
    let forgejo = render_at(72, 18, &app);
    assert!(
        forgejo.contains("press c"),
        "the role check is below the fold:\n{forgejo}"
    );
}

#[test]
fn the_instruction_screen_previews_the_document_and_the_steps_command() {
    let root = scratch("instruction-preview");
    let mut app = app_with(&root, ONE_AGENT);
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(
        paths.instruction(),
        "Read the recent commits and draft release notes.\n",
    )
    .expect("the instruction is written");
    fs::write(
        paths.steps_file(),
        "git -C release-notes log --oneline -12\n",
    )
    .expect("the steps file is written");
    app.reload();
    app.show(Screen::Instructions);
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("preview"),
        "no read-only preview:\n{screen}"
    );
    assert!(screen.contains("Read the recent commits"), "{screen}");
    assert!(
        screen.contains("git -C release-notes log"),
        "the steps command is not previewed:\n{screen}"
    );
    assert!(
        screen.contains("run in the workspace at"),
        "the working directory is not explicit:\n{screen}"
    );
    assert!(
        screen.contains("workspace/<repository>"),
        "the clone position is not explained:\n{screen}"
    );
}

#[test]
fn the_hub_state_column_shows_the_failure_class_whole() {
    let root = scratch("state-whole");
    let mut app = app_with(&root, ONE_AGENT);
    write_run(
        &root,
        "a reply",
        Some("the model call failed (server_error): the endpoint answered HTTP 500"),
    );
    app.reload();
    app.show(Screen::Agents);
    let screen = render_at(80, 24, &app);
    assert!(screen.contains("failed (server_error)"), "{screen}");
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains("failed (server_error)"),
        "the state is shortened at the floor:\n{floor}"
    );
}

// 4. One colour scheme -----------------------------------------------------

#[test]
fn errors_warnings_information_and_selection_use_one_scheme() {
    let root = scratch("colour");
    fs::write(root.join("config.toml"), "[[agents]\n").expect("a broken file");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("colour-broken"),
    );
    app.show_acknowledgment = false;
    let (fg, bg, modifier) = cell_style_after(&app, 160, 40, "no agent list");
    assert_eq!(
        fg,
        Color::Rgb(255, 95, 95),
        "the error is not the measured red"
    );
    assert_eq!(
        bg,
        Color::Rgb(0, 0, 0),
        "the error has no painted background"
    );
    assert!(modifier.contains(Modifier::BOLD), "the error is not bold");

    let root = scratch("colour-warning");
    fs::write(root.join("config.toml"), EMPTY_CONFIG).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        quiet_runner("colour-warning"),
    );
    app.show_acknowledgment = false;
    let (fg, bg, modifier) = cell_style_after(&app, 160, 40, "The risk of running an agent");
    assert_eq!(
        fg,
        Color::Rgb(255, 215, 0),
        "the warning is not the measured yellow"
    );
    assert_eq!(
        bg,
        Color::Rgb(0, 0, 0),
        "the warning has no painted background"
    );
    assert!(modifier.contains(Modifier::BOLD), "the warning is not bold");

    let root = scratch("colour-footer");
    let mut app = app_with(&root, EMPTY_CONFIG);
    app.status =
        "endpoint test for `scribe`: failed (transport): the endpoint refused the connection"
            .to_owned();
    let (fg, bg, modifier) = cell_style_after(&app, 160, 40, "endpoint test for");
    assert_eq!(
        fg,
        Color::Rgb(255, 95, 95),
        "a transport failure is uncoloured in the footer"
    );
    assert_eq!(bg, Color::Rgb(0, 0, 0));
    assert!(modifier.contains(Modifier::BOLD));

    app.status = "leaving stops the 1 run this UI started; press q again to quit".to_owned();
    app.confirm_quit = Some(1);
    let (fg, bg, modifier) = cell_style_after(&app, 160, 40, "leaving stops");
    assert_eq!(
        fg,
        Color::Rgb(255, 215, 0),
        "the quit warning is uncoloured in the footer"
    );
    assert_eq!(bg, Color::Rgb(0, 0, 0));
    assert!(modifier.contains(Modifier::BOLD));

    app.confirm_quit = None;
    let root = scratch("colour-selection");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Agents);
    let (fg, bg, modifier) = cell_style_after(&app, 120, 40, ">  scribe");
    assert_eq!(
        (fg, bg),
        (Color::Black, Color::Rgb(0, 205, 205)),
        "the selection cue changed"
    );
    assert!(
        modifier.contains(Modifier::BOLD),
        "the selection is not bold"
    );
}

// 5. Role and identity truth ----------------------------------------------

#[test]
fn an_administrator_token_supplied_in_the_form_is_not_demanded_again() {
    let root = scratch("form-admin");
    let mut app = app_with(&root, EMPTY_CONFIG);
    app.on_key(key(KeyCode::Char('n')));
    {
        let form = app.add_agent.as_mut().expect("the form is open");
        form.fields[0] = "scribe".to_owned();
        form.fields[1] = "https://model.example.org/v1".to_owned();
        form.fields[2] = "example-model".to_owned();
        form.fields[3] = "gw-fixture-key".to_owned();
        form.fields[4] = "https://forgejo.example.org".to_owned();
        form.fields[5] = "gw-admin-token".to_owned();
        if let Some(role) = form.fields.get_mut(6) {
            *role = "admin".to_owned();
        }
    }
    app.on_key(ctrl_s());
    assert!(
        !app.adding_agent(),
        "the form did not write: {}",
        app.status
    );
    let credentials = fs::read_to_string(&app.credentials_path).expect("credentials");
    assert!(credentials.contains("forgejo_admin_token"), "{credentials}");
    let config = fs::read_to_string(app.config_path()).expect("configuration");
    assert!(config.contains("role = \"admin\""), "{config}");
    assert!(
        app.forgejo_tokens_present().1,
        "the administrator token is not seen"
    );
}

#[test]
fn the_role_check_does_not_offer_a_dead_c_and_the_identity_says_what_it_does() {
    let root = scratch("role-truth");
    let mut app = app_with(&root, ONE_AGENT);
    app.role_check = Some(Ok(RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "admin-user".to_owned(),
            email: None,
            description: None,
            is_admin: true,
        },
        repositories: Vec::new(),
        private_visible: 0,
    }));
    app.show(Screen::Forgejo);
    let screen = render_at(120, 40, &app);
    assert!(
        !screen.contains("press C for that view"),
        "a dead C is offered:\n{screen}"
    );
    assert!(
        screen.contains("press A on the Credentials screen"),
        "{screen}"
    );

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
    app.show(Screen::AgentConfig);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("commits, pull requests and issues"),
        "{screen}"
    );
}

#[test]
fn the_systemd_refusal_names_the_route_to_a_reachable_user_manager() {
    let root = scratch("systemd-route");
    let layout = Layout::rooted_at(&root);
    let agent = layout
        .ensure_agent("scribe")
        .expect("the agent is prepared");
    let mut supervisor = Supervisor::new(
        layout.state_root(),
        PathBuf::from("/usr/local/bin/gewu-multiverse"),
        10,
        Duration::from_secs(30),
    );
    supervisor.set_systemctl(Systemctl::with_binary("/nonexistent/systemctl-not-here"));
    let error = supervisor
        .switch_mode(&agent, Mode::Systemd)
        .expect_err("the manager is unreachable");
    assert!(error.contains("loginctl enable-linger"), "{error}");
    assert!(error.contains("login user session"), "{error}");
}

// 6. No host identity on screens ------------------------------------------

#[test]
fn no_screen_prints_the_host_name_or_machine_id() {
    let Some(mark) = gewu_multiverse::machine::mark() else {
        return;
    };
    let root = scratch("no-host");
    let mut app = app_with(&root, ONE_AGENT);
    app.role_checked_at_ms = Some(gewu_multiverse::time::now_unix_ms());
    app.role_check_machine = Some(mark.clone());
    app.show(Screen::Forgejo);
    let screen = render_at(160, 50, &app);
    assert!(
        !screen.contains(&mark),
        "the machine mark is on screen:\n{screen}"
    );
    assert!(
        screen.contains("this machine"),
        "the neutral label is missing:\n{screen}"
    );
}

// 7. Docs and copy ---------------------------------------------------------

#[test]
fn the_readme_has_no_stale_manual_release_and_says_there_is_no_scheduler() {
    let readme = include_str!("../README.md");
    assert!(
        !readme.contains("v0.1.24"),
        "the README still names the old release"
    );
    assert!(
        !readme.contains("8594ff672d49"),
        "the README still names the old tarball"
    );
    assert!(
        readme.to_ascii_lowercase().contains("no scheduler"),
        "the README does not state the scheduler boundary"
    );
    assert!(
        readme.contains("release page"),
        "the README does not point at the release page"
    );
    let map = include_str!("../docs/page-map.md");
    assert!(
        map.contains("press ]"),
        "the page map still names the wrong overflow advice"
    );
    assert!(
        map.contains("failed (server_error)"),
        "the page map does not keep the class whole"
    );
}
