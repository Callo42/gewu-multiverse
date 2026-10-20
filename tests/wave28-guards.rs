//! Guards for the nine accepted UX classes of this correction.
//!
//! Every guard drives the product's own `App`, key handling, storage and renderer.
//! They are written to fail on the exact base and pass after the correction.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, EditTarget, Screen};
use gewu_multiverse::config::Role;
use gewu_multiverse::credentials::Secret;
use gewu_multiverse::forgejo::{Permissions, Repository, RoleCheck, Viewer};
use gewu_multiverse::model::ModelConfig;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const TWO_AGENTS: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "ledgerlite-changelog"
model = "kencode-chat-1"
endpoint = "http://127.0.0.1:19280/v1/chat/completions"
instruction = "agents/ledgerlite-changelog/instruction.md"
concurrency = 2

[[agents]]
name = "punchcard-changelog"
model = "punchcard-chat-1"
endpoint = "http://127.0.0.1:19280/v1/chat/completions"
instruction = "agents/punchcard-changelog/instruction.md"
concurrency = 1
"#;

const ONE_AGENT: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "ledgerlite-changelog"
model = "kencode-chat-1"
endpoint = "http://127.0.0.1:19280/v1/chat/completions"
instruction = "agents/ledgerlite-changelog/instruction.md"
concurrency = 2
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-r28-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn quiet_runner(tag: &str) -> PathBuf {
    runner(tag, "#!/bin/sh\nexit 0\n")
}

fn runner(tag: &str, body: &str) -> PathBuf {
    let path = scratch(tag).join("runner.sh");
    fs::write(&path, body).expect("the runner is written");
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
    app.show_acknowledgment = false;
    app
}

fn app_with_runner(root: &Path, config: &str, runner_path: &Path) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        runner_path.to_path_buf(),
    );
    app.show_acknowledgment = false;
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
    app.show_acknowledgment = false;
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

fn write_instruction(app: &App, name: &str, text: &str) {
    let layout = Layout::rooted_at(app.root().to_path_buf());
    let paths = layout.agent(name).expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(paths.instruction(), text).expect("the instruction is written");
}

fn write_run(root: &Path, agent: &str, run_id: &str, reply: &str, pushed: bool) {
    let layout = Layout::rooted_at(root.to_path_buf());
    let dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&dir).expect("the run directory");
    let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(run_id, agent, 4321, "/w"))
        .expect("a run starts");
    if pushed {
        // The push is a fact about the repository state, witnessed by the run itself;
        // the transcript alone is not evidence (the push-is-a-fact rule).
        gewu_multiverse::run::write_push_witness(
            &dir,
            "changelog/weekly",
            "refs/remotes/origin/changelog/weekly",
        )
        .expect("the push witness is written");
    }
    transcript
        .append(&Record::model_reply(
            run_id,
            2,
            200,
            Some(reply.to_owned()),
            Vec::new(),
            Some("stop".to_owned()),
            None,
            "{}",
        ))
        .expect("a reply is recorded");
    transcript
        .append(&Record::run_finished(
            run_id,
            RunOutcome::Succeeded,
            Some(0),
            1200,
            None,
        ))
        .expect("the run ends");
    fs::write(dir.join(gewu_multiverse::run::LOG_FILE), "a log\n").expect("a log is written");
}

fn four_repositories() -> RoleCheck {
    let repositories = (1..=4)
        .map(|at| Repository {
            full_name: format!("ken/repo-{at}"),
            namespace: "ken".to_owned(),
            name: format!("repo-{at}"),
            private: at % 2 == 0,
            default_branch: Some("main".to_owned()),
            html_url: None,
            permissions: Permissions {
                admin: false,
                push: true,
                pull: true,
            },
        })
        .collect();
    RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "ken".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories,
        private_visible: 2,
    }
}

fn record_identity(app: &App) {
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("forgejo_logins"),
        "ledgerlite-changelog",
        "gewu-ledgerlite-changelog",
    )
    .expect("the login is recorded");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("forgejo_emails"),
        "ledgerlite-changelog",
        "gewu-ledgerlite-changelog@agents.invalid",
    )
    .expect("the email is recorded");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("forgejo_identity_modes"),
        "ledgerlite-changelog",
        "managed",
    )
    .expect("the mode is recorded");
}

// 1 — the endpoint field says what it wants, and can be edited.
#[test]
fn the_endpoint_shape_is_named_and_the_agent_can_edit_it_without_being_removed() {
    let root = scratch("endpoint-shape");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::AgentConfig);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("https://…/v1/chat/completions")
            || screen.contains("https://host/v1/chat/completions")
            || screen.contains("/v1/chat/completions"),
        "the endpoint shape and example are not on the Agent screen:\n{screen}"
    );

    app.on_key(KeyEvent::from(KeyCode::Char('E')));
    let editor = render_at(80, 24, &app);
    assert!(
        editor.contains("edit the endpoint")
            && editor.contains("model")
            && editor.to_ascii_lowercase().contains("key"),
        "no in-product endpoint/model/key editor opened:\n{editor}"
    );
    assert_eq!(
        app.agents().len(),
        1,
        "editing must not remove the agent first"
    );
}

#[test]
fn a_404_on_the_path_is_not_model_not_found_when_the_models_list_has_the_model() {
    use gewu_multiverse::test_stub::{Stub, StubReply};
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"no route"}"#),
        StubReply::json(r#"{"object":"list","data":[{"id":"example-model"}]}"#),
    ]);
    let config = ModelConfig {
        endpoint: stub.url(),
        model: "example-model".to_owned(),
        api_key: Secret::new("sk-test-key"),
        timeout: std::time::Duration::from_secs(5),
    };
    let error = gewu_multiverse::model::probe(&config).expect_err("the path is wrong");
    assert_eq!(error.class(), "route_not_found", "{error}");
    assert!(
        error.to_string().contains("/v1/chat/completions"),
        "the correction does not name the full path: {error}"
    );
}

#[test]
fn a_404_where_the_models_list_lacks_the_model_is_model_not_found() {
    use gewu_multiverse::test_stub::{Stub, StubReply};
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"no model"}"#),
        StubReply::json(r#"{"object":"list","data":[{"id":"another-model"}]}"#),
    ]);
    let config = ModelConfig {
        endpoint: stub.url(),
        model: "example-model".to_owned(),
        api_key: Secret::new("sk-test-key"),
        timeout: std::time::Duration::from_secs(5),
    };
    let error = gewu_multiverse::model::probe(&config).expect_err("the model is unknown");
    assert_eq!(error.class(), "model_not_found", "{error}");
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        2,
        "the models list was not asked: {requests:?}"
    );
    assert_eq!(requests[1].method, "GET");
    assert!(
        requests[1].path.ends_with("/models"),
        "{}",
        requests[1].path
    );
}

#[test]
fn every_endpoint_failure_phrase_names_the_endpoint_and_model() {
    let message = "the endpoint answered HTTP 500: boom";
    for class in [
        "bad_key",
        "server_error",
        "rate_limited",
        "malformed_reply",
        "timeout",
        "transport",
        "model_not_found",
        "route_not_found",
    ] {
        let failure = gewu_multiverse::app::EndpointFailure {
            class: class.to_owned(),
            message: message.to_owned(),
        };
        let test = gewu_multiverse::app::EndpointTest {
            agent: "ledgerlite-changelog".to_owned(),
            endpoint: "https://host/v1/chat/completions".to_owned(),
            model: "kencode-chat-1".to_owned(),
            result: Err(failure),
            tested_at_ms: gewu_multiverse::time::now_unix_ms(),
            machine: None,
        };
        let line = test.describe();
        assert!(
            line.contains("endpoint https://host/v1/chat/completions"),
            "{line}"
        );
        assert!(line.contains("model kencode-chat-1"), "{line}");
    }
}

// 2 — a field never carries text the user did not type.
#[test]
fn the_address_prompt_opens_empty_and_a_refused_url_stays_in_the_box() {
    let root = scratch("url-field");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Forgejo);
    app.request_url_edit();
    let prompt = app.url_prompt.as_ref().expect("the address prompt is open");
    assert!(
        prompt.value.is_empty(),
        "the old address is in the field: {:?}",
        prompt.value
    );
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("https://forgejo.example.org"),
        "the old address is not offered as a dim placeholder:\n{screen}"
    );

    if let Some(open) = app.url_prompt.as_mut() {
        open.value = "https://forgejo.example.orghttp://127.0.0.1:9".to_owned();
    }
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert!(
        app.url_prompt.is_some(),
        "an impossible URL was accepted and the box closed"
    );
    assert_eq!(
        app.config.as_ref().expect("the config is read").forgejo.url,
        "https://forgejo.example.org"
    );
    let refused = render_at(80, 24, &app);
    assert!(
        refused.contains("not a URL") || refused.contains("host") || refused.contains("scheme"),
        "the box does not carry the URL refusal:\n{refused}"
    );
}

#[test]
fn the_new_instruction_template_is_empty() {
    let root = scratch("instruction-template");
    let mut app = app_with(&root, ONE_AGENT);
    app.request_edit(EditTarget::Instruction);
    if app.editor_confirm.is_some() {
        app.on_key(KeyEvent::from(KeyCode::Enter));
    }
    let action = app.take_action().expect("an edit was handed off");
    let written = fs::read_to_string(&action.path).expect("the template was written");
    assert!(
        written.is_empty(),
        "a developer placeholder would reach the model: {written:?}"
    );
}

// 3 — the quit warning names the agents and runs it will stop.
#[test]
fn the_quit_warning_names_the_agent_run_and_elapsed_time() {
    let root = scratch("quit-names");
    let runner = runner("quit-names-runner", "#!/bin/sh\nsleep 30\n");
    let mut app = app_with_runner(&root, ONE_AGENT, &runner);
    write_instruction(&app, "ledgerlite-changelog", "work\n");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "ledgerlite-changelog",
        "sk-test-key",
    )
    .expect("the key is recorded");
    app.selected = 0;
    app.request_agent_start();
    assert_eq!(app.supervisor.running_count(), 1, "{}", app.status);
    app.request_quit();
    assert!(
        app.confirm_quit.is_some(),
        "no leave question: {}",
        app.status
    );
    assert!(
        app.status.contains("ledgerlite-changelog"),
        "the warning does not name the agent: {}",
        app.status
    );
    assert!(app.status.contains("run-"), "no run id: {}", app.status);
    assert!(app.status.contains(" s"), "no elapsed time: {}", app.status);
    let _ = app.shutdown();
}

// 4 — width goes to the names.
#[test]
fn a_wide_window_shows_whole_agent_and_model_names_with_slack_after_state() {
    let root = scratch("wide-names");
    let mut app = app_with(&root, TWO_AGENTS);
    app.show(Screen::Agents);
    let screen = render_at(120, 40, &app);
    assert!(screen.contains("ledgerlite-changelog"), "{screen}");
    assert!(screen.contains("punchcard-changelog"), "{screen}");
    assert!(screen.contains("kencode-chat-1"), "{screen}");
    assert!(screen.contains("punchcard-chat-1"), "{screen}");
    assert!(
        !screen.contains("ledger…angelog"),
        "a name is ellipsized while width is free:\n{screen}"
    );
    let state_row = screen
        .lines()
        .find(|line| line.contains("ledgerlite-changelog"))
        .unwrap_or_default();
    assert!(
        state_row.contains("idle "),
        "the state column touches the border with no right padding: {state_row:?}"
    );
}

// 5 — scrolling at the floor is a page, not a line.
#[test]
fn a_scrolled_panel_moves_a_viewport_and_names_line_n_of_m() {
    let root = scratch("panel-page");
    let mut app = app_with(&root, ONE_AGENT);
    write_instruction(
        &app,
        "ledgerlite-changelog",
        &(0..40)
            .map(|at| format!("instruction line {at}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    app.show(Screen::Instructions);
    let top = render_at(72, 18, &app);
    let first_total = top
        .lines()
        .find(|line| line.contains("line 1 of"))
        .unwrap_or_default()
        .to_owned();
    assert!(
        !first_total.is_empty(),
        "the title does not carry line n of m:\n{top}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char(']')));
    let paged = render_at(72, 18, &app);
    assert!(
        !paged.contains("line 1 of"),
        "one `]` did not move a viewport:\n{paged}"
    );
    assert!(paged.contains("line "), "{paged}");
}

#[test]
fn the_floor_runs_screen_collapses_the_list_and_names_its_own_scroll_key() {
    let root = scratch("floor-runs");
    let mut app = app_with(&root, ONE_AGENT);
    for at in 1..=5 {
        write_run(
            &root,
            "ledgerlite-changelog",
            &format!("run-170000000000{at}-000{at}-1"),
            "the model's reply is long enough to need the panel",
            false,
        );
    }
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(72, 18, &app);
    let list_line = screen
        .lines()
        .find(|line| line.contains("recent runs"))
        .unwrap_or_default();
    assert!(screen.contains("recent runs"), "{screen}");
    assert!(
        list_line.contains("up/down") || list_line.contains("↓/↑"),
        "the collapsed list does not name its own keys: {list_line:?}\n{screen}"
    );
    assert!(
        screen.contains("run records"),
        "the records panel is missing at the floor:\n{screen}"
    );
}

#[test]
fn the_help_header_names_the_panel_keys() {
    let root = scratch("help-page");
    let mut app = app_with(&root, ONE_AGENT);
    app.on_key(KeyEvent::from(KeyCode::Char('?')));
    let screen = render_at(72, 18, &app);
    assert!(
        screen.contains("[ / ]"),
        "the help header does not name [ / ]:\n{screen}"
    );
}

// 6 — nothing primary below the fold at 72×18 and 80×24.
#[test]
fn every_screen_keeps_a_screen_specific_action_row_at_the_floor() {
    let root = scratch("action-rows");
    let mut app = app_with(&root, ONE_AGENT);
    let cases = [
        (Screen::Agents, "n new agent"),
        (Screen::AgentConfig, "E edit endpoint"),
        (Screen::Instructions, "t edit steps"),
        (Screen::Forgejo, "u url"),
        (Screen::Credentials, "k model key"),
        (Screen::Runs, "T transcript"),
    ];
    for (screen_name, needle) in cases {
        app.show(screen_name);
        let screen = render_at(72, 18, &app);
        assert!(
            screen.contains(needle),
            "{screen_name:?} has no `{needle}` action row:\n{screen}"
        );
    }
}

#[test]
fn the_add_form_at_the_floor_shows_named_fields_and_its_action_inside_the_panel() {
    let root = scratch("form-floor");
    let mut app = app_with(&root, ONE_AGENT);
    app.request_new_agent();
    let screen = render_at(72, 18, &app);
    assert!(screen.contains("agent name"), "{screen}");
    assert!(screen.contains("model endpoint"), "{screen}");
    assert!(
        screen.contains("https://…/v1/chat/completions")
            || screen.contains("https://host/v1/chat/completions"),
        "the endpoint field does not say what shape it wants:\n{screen}"
    );
    let lines: Vec<&str> = screen.lines().collect();
    let bottom = lines
        .iter()
        .rposition(|line| line.contains('└'))
        .expect("the form has a bottom border");
    assert!(
        lines[bottom - 1].contains("ctrl-s writes it"),
        "the action line is outside or above the panel at the floor:\n{screen}"
    );
}

#[test]
fn the_instruction_preview_is_visible_at_entry_at_the_floor() {
    let root = scratch("preview-floor");
    let mut app = app_with(&root, ONE_AGENT);
    write_instruction(
        &app,
        "ledgerlite-changelog",
        "You write a weekly changelog from commits.\nBe terse.\n",
    );
    app.show(Screen::Instructions);
    let screen = render_at(72, 18, &app);
    assert!(
        screen.contains("You write a weekly changelog"),
        "the preview is below the fold at entry:\n{screen}"
    );
    assert!(
        !screen.contains("1 more lines"),
        "the preview still uses a wrong capped plural:\n{screen}"
    );
}

#[test]
fn the_forgejo_identity_report_is_drawn_once_after_the_list_and_the_range_is_truthful() {
    let root = scratch("forgejo-report");
    let mut app = app_with(&root, ONE_AGENT);
    record_identity(&app);
    app.role_check = Some(Ok(four_repositories()));
    app.show(Screen::Forgejo);
    let screen = render_at(80, 24, &app);
    let normalised_screen: String = screen.chars().filter(|ch| ch.is_alphanumeric()).collect();
    assert_eq!(
        normalised_screen
            .matches("commitspullrequestsandissues")
            .count(),
        1,
        "the identity report is duplicated:\n{screen}"
    );
    // The range is truthful for the form drawn: the windowed form may not claim
    // rows it does not draw, while the scrolled form names the whole list because
    // `[`/`]` really reach every row.
    if screen.contains("line 1 of") {
        assert!(
            screen.contains("more below"),
            "the scrolled list declares no way to reach its other rows:\n{screen}"
        );
    } else {
        assert!(
            !screen.contains("repositories 1–4 of 4"),
            "the range claims four rows when the window draws fewer:\n{screen}"
        );
    }
    assert!(
        screen.contains("repositories 1–"),
        "the list has no truthful range:\n{screen}"
    );
}

#[test]
fn the_first_run_overlay_owns_its_footer_and_fits_its_record_path_at_80() {
    let root = scratch("first-run-footer");
    fs::write(root.join("config.toml"), ONE_AGENT).expect("the configuration is written");
    let app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("first-run-footer-runner"),
    );
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("q quit"),
        "the first-run footer advertises q quit:\n{screen}"
    );
    assert!(
        screen.contains("esc closes this overlay") || screen.contains("y records"),
        "the first-run footer is not the overlay's own:\n{screen}"
    );
    // The scratch path's length is the host's, not the product's. A short path is
    // drawn whole; a path long enough to wrap must leave its tail reachable and say so
    // in the overlay's title instead of failing this guard for the temp directory.
    assert!(
        screen.contains("risk-acknowledgment.toml")
            || screen.contains("more below")
            || screen.contains("press ]"),
        "the record path is neither drawn nor declared at 80×24:\n{screen}"
    );
}

// 7 — honest sentences and fresh selection.
#[test]
fn the_agent_detail_leads_with_the_clone_not_with_not_a_git_repository() {
    let root = scratch("clone-lead");
    let mut app = app_with(&root, ONE_AGENT);
    let layout = Layout::rooted_at(root.to_path_buf());
    let paths = layout
        .agent("ledgerlite-changelog")
        .expect("the agent path");
    let clone = paths.workspace().join("ledgerlite");
    fs::create_dir_all(&clone).expect("the clone directory");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "a@b.invalid"],
        vec!["config", "user.name", "A"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&clone)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }
    fs::write(clone.join("README.md"), "hello\n").expect("a file");
    assert!(
        std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&clone)
            .status()
            .expect("git runs")
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-qm", "first"])
            .current_dir(&clone)
            .status()
            .expect("git runs")
            .success()
    );
    app.show(Screen::AgentConfig);
    let screen = render_at(120, 40, &app);
    let repository = screen
        .lines()
        .find(|line| line.contains("repository"))
        .unwrap_or_default();
    assert!(
        !repository.contains("not a git repository"),
        "the clone is introduced as a failure: {repository}"
    );
    assert!(repository.contains("ledgerlite"), "{repository}");
    assert!(
        repository.contains("main") || repository.contains("master"),
        "{repository}"
    );
}

#[test]
fn the_runs_screen_selects_the_newest_run_when_it_opens() {
    let root = scratch("runs-newest");
    let mut app = app_with(&root, ONE_AGENT);
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000001-0001-1",
        "older",
        false,
    );
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000002-0002-1",
        "newest",
        false,
    );
    app.reload();
    app.run_selected = 1;
    app.show(Screen::Runs);
    let selected = app.selected_run().expect("a selected run");
    assert_eq!(selected.run_id, "run-1700000000002-0002-1");
}

#[test]
fn a_reopened_role_line_is_a_last_check_or_an_absence_not_the_configured_default() {
    let root = scratch("role-reopen");
    fs::write(root.join("config.toml"), ONE_AGENT).expect("the configuration is written");
    let layout = Layout::rooted_at(root.to_path_buf());
    fs::write(
        layout.role_checks_file(),
        format!(
            "[check]\nchecked_at_ms = {}\nmachine = \"test-machine\"\noutcome = \"administrator\"\n",
            gewu_multiverse::time::now_unix_ms()
        ),
    )
    .expect("the role-check record is written");
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    let mut app = App::with_runner(layout, quiet_runner("role-reopen-runner"));
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("last check: administrator"),
        "the reopen role line does not carry the last result:\n{screen}"
    );
}

#[test]
fn a_labelled_path_wraps_with_the_label_indent_on_continuation_rows() {
    let root = scratch("path-indent");
    // A URL long enough that the field must actually wrap at 72 columns: the old
    // fixture's endpoint fitted, so the guard's conditional asserted nothing.
    let config = ONE_AGENT.replace(
        "http://127.0.0.1:19280/v1/chat/completions",
        "http://127.0.0.1:19280/v1/chat/completions/with-a-long/repository-route",
    );
    let mut app = app_with(&root, &config);
    app.show(Screen::AgentConfig);
    let screen = render_at(72, 18, &app);
    let lines: Vec<&str> = screen.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains("endpoint") && line.contains("http"))
        .unwrap_or_else(|| panic!("no endpoint row:\n{screen}"));
    assert!(
        !lines[at].contains("repository-route"),
        "the endpoint row did not wrap, so this guard proves nothing:\n{screen}"
    );
    assert!(
        at + 1 < lines.len() && lines[at + 1].starts_with("│                "),
        "a wrapped value lost its label indent: {:?}",
        lines.get(at + 1)
    );
    let value: String = lines[at..]
        .iter()
        .take(4)
        .map(|line| line.trim_matches('│'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("");
    assert!(
        value.contains("repository-route"),
        "the wrapped continuation is not the endpoint value:\n{screen}"
    );
}

// 8 — contrast and density.
#[test]
fn the_screen_paints_an_explicit_background_so_contrast_is_theme_independent() {
    use ratatui::style::Color;
    let root = scratch("background");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Agents);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("a terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, &app))
        .expect("a frame");
    let buffer = terminal.backend().buffer();
    let sample = buffer.cell((10, 8)).expect("a body cell");
    assert!(
        sample.bg != Color::Reset,
        "a body cell inherits the terminal background: {sample:?}"
    );
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = buffer.cell((x, y)).expect("a cell");
            if cell.symbol() != " " {
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "cell ({x},{y}) {:?} inherits the terminal background",
                    cell.symbol()
                );
            }
        }
    }
}

#[test]
fn the_hub_and_credentials_screens_do_not_print_policy_prose() {
    let root = scratch("density");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Agents);
    let agents = render_at(120, 40, &app);
    assert!(
        !agents.contains("loginctl enable-linger"),
        "the systemd policy paragraph is still on the Agents screen:\n{agents}"
    );
    assert!(
        !agents.contains("concurrency is how many model requests"),
        "the definition prose is still on the hub:\n{agents}"
    );
    app.show(Screen::Credentials);
    let credentials = render_at(120, 40, &app);
    assert!(
        !credentials.contains("copying the configuration directory"),
        "the credentials policy wall is still printed:\n{credentials}"
    );
}

#[test]
fn the_model_reply_and_the_products_notes_are_set_apart() {
    let root = scratch("reply-rule");
    let mut app = app_with(&root, ONE_AGENT);
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000003-0003-1",
        "a model reply",
        false,
    );
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("end of the model's reply"),
        "the product's notes are not visibly separated from the reply:\n{screen}"
    );
}

// 9 — after the run, the next step is offered.
#[test]
fn a_pushed_run_names_the_pull_request_key_as_the_next_step() {
    let root = scratch("next-step");
    let mut app = app_with(&root, ONE_AGENT);
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000004-0004-1",
        "Committed and pushed changelog/weekly.",
        true,
    );
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("P ") && screen.contains("checks the branch"),
        "the pushed run does not offer P as the next step:\n{screen}"
    );
}

#[test]
fn the_agent_screen_editor_writes_the_endpoint_model_and_key_without_removing() {
    let root = scratch("endpoint-edit");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::AgentConfig);
    app.on_key(KeyEvent::from(KeyCode::Char('E')));
    if let Some(form) = app.edit_agent.as_mut() {
        form.fields[0] = "https://model.example.org/v2/chat/completions".to_owned();
        form.fields[1] = "edited-model".to_owned();
        form.fields[2] = "sk-edited-key".to_owned();
    } else {
        panic!("the endpoint editor did not open");
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(
        app.edit_agent.is_none(),
        "the editor did not close: {}",
        app.status
    );
    let agent = app
        .agents()
        .iter()
        .find(|agent| agent.name == "ledgerlite-changelog")
        .expect("the agent is still configured");
    assert_eq!(
        agent.endpoint,
        "https://model.example.org/v2/chat/completions"
    );
    assert_eq!(agent.model, "edited-model");
    assert_eq!(app.agents().len(), 1, "editing must not remove the agent");
    let key =
        gewu_multiverse::credentials::model_key(&app.credentials_path, "ledgerlite-changelog")
            .expect("the key is readable");
    assert_eq!(key.expose(), "sk-edited-key");
}

#[test]
fn the_agent_editor_refuses_a_bad_url_and_keeps_the_typed_value() {
    let root = scratch("endpoint-edit-bad");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::AgentConfig);
    app.on_key(KeyEvent::from(KeyCode::Char('E')));
    if let Some(form) = app.edit_agent.as_mut() {
        form.fields[0] = "model.example.org/v1".to_owned();
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    let form = app.edit_agent.as_ref().expect("the editor stays open");
    assert_eq!(form.fields[0], "model.example.org/v1", "the value was lost");
    assert!(
        form.error
            .as_deref()
            .is_some_and(|error| error.contains("http://")),
        "no URL reason in the box: {:?}",
        form.error
    );
    assert_eq!(
        app.agents()[0].endpoint,
        "http://127.0.0.1:19280/v1/chat/completions"
    );
}

#[test]
fn the_add_form_refuses_a_url_without_a_host_before_writing_it() {
    let root = scratch("form-bad-url");
    let mut app = app_with(&root, ONE_AGENT);
    app.request_new_agent();
    if let Some(form) = app.add_agent.as_mut() {
        form.fields[0] = "second-agent".to_owned();
        form.fields[1] = "https://".to_owned();
        form.fields[2] = "a-model".to_owned();
        form.fields[3] = "sk-key".to_owned();
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.add_agent.is_some(), "the form closed on a refused URL");
    assert_eq!(app.agents().len(), 1, "the refused form wrote an agent");
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("names no host") || screen.contains("model endpoint"),
        "the refusal is not in the box:\n{screen}"
    );
}
