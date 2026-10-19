//! Guards for the overlay bands, the endpoint editor at the floor, honest role and run
//! history, the one paging rule and the push fact.
//!
//! Every guard drives the product's own `App`, key handling, storage, supervisor and
//! renderer.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, EditTarget, Screen};
use gewu_multiverse::config::Role;
use gewu_multiverse::forgejo::{RoleCheck, Viewer};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, RunPlan};
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

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
    let root = std::env::temp_dir().join(format!("gewu-c28-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn quiet_runner(tag: &str) -> PathBuf {
    runner(tag, "#!/bin/sh\nexit 0\n")
}

fn runner(tag: &str, body: &str) -> PathBuf {
    let path = scratch(tag).join("runner.sh");
    fs::write(&path, body).expect("the runner is written");
    use std::os::unix::fs::PermissionsExt;
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

/// Every cell of an overlay frame, spaces included, must carry the product's own
/// background. `Clear` resets cells to the terminal default, so this is what catches
/// the unpainted side bands a light terminal shows as white strips.
fn assert_every_cell_painted(app: &App, width: u16, height: u16, label: &str) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, app))
        .expect("a frame is drawn");
    let buffer = terminal.backend().buffer();
    for y in 0..height {
        for x in 0..width {
            let cell = buffer.cell((x, y)).expect("a cell");
            assert_ne!(
                cell.bg,
                Color::Reset,
                "{label} at {width}×{height}: cell ({x},{y}) {:?} inherits the terminal background",
                cell.symbol()
            );
        }
    }
}

fn write_instruction(app: &App, name: &str, text: &str) {
    let layout = Layout::rooted_at(app.root().to_path_buf());
    let paths = layout.agent(name).expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(paths.instruction(), text).expect("the instruction is written");
}

fn write_run(root: &Path, agent: &str, run_id: &str, reply: &str) {
    let layout = Layout::rooted_at(root.to_path_buf());
    let dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&dir).expect("the run directory");
    let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(run_id, agent, 4321, "/w"))
        .expect("a run starts");
    transcript
        .append(&Record::model_reply(
            run_id,
            1,
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

fn empty_role_check() -> RoleCheck {
    RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "ken".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: Vec::new(),
        private_visible: 0,
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// B1 — the cleared overlay band is painted, spaces included, on every overlay frame.
#[test]
fn every_overlay_paints_every_cell_including_the_side_bands() {
    let sizes = [(72u16, 18u16), (80, 24), (120, 40)];
    let root = scratch("overlay-bg");

    // The first-run acknowledgment needs a configuration with no record yet.
    let unack_root = scratch("overlay-first-run");
    fs::write(unack_root.join("config.toml"), ONE_AGENT).expect("the configuration");
    let unack = App::with_runner(
        Layout::rooted_at(unack_root),
        quiet_runner("overlay-first-run-runner"),
    );
    for (width, height) in sizes {
        assert_every_cell_painted(&unack, width, height, "first-run acknowledgment");

        let mut app = app_with(&root, ONE_AGENT);
        app.request_new_agent();
        assert_every_cell_painted(&app, width, height, "add-agent form");

        let mut app = app_with(&root, ONE_AGENT);
        app.show(Screen::AgentConfig);
        app.on_key(KeyEvent::from(KeyCode::Char('E')));
        assert_every_cell_painted(&app, width, height, "endpoint editor");

        let mut app = app_with(&root, ONE_AGENT);
        app.review_acknowledgment();
        assert_every_cell_painted(&app, width, height, "read-only terms");

        let mut app = app_with(&root, ONE_AGENT);
        app.request_remove_agent();
        assert_every_cell_painted(&app, width, height, "removal confirmation");

        let mut app = app_with(&root, ONE_AGENT);
        app.show(Screen::AgentConfig);
        app.request_edit(EditTarget::Instruction);
        assert_every_cell_painted(&app, width, height, "editor confirmation");

        let mut app = app_with(&root, ONE_AGENT);
        gewu_multiverse::credentials::record_secret(
            &app.credentials_path,
            Some("model_keys"),
            "an-agent-that-is-gone",
            "sk-leftover",
        )
        .expect("the leftover key is recorded");
        app.reload();
        app.request_forget_secrets();
        assert_every_cell_painted(&app, width, height, "leftover-secret confirmation");

        let mut app = app_with(&root, ONE_AGENT);
        app.toggle_help();
        assert_every_cell_painted(&app, width, height, "key help");
    }
}

// B2 — the role outcome is persisted from the result, and a URL change drops it.
#[test]
fn a_failed_role_check_is_the_reopened_result_and_a_url_change_drops_it() {
    let root = scratch("role-history");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Forgejo);
    app.on_key(KeyEvent::from(KeyCode::Char('c')));
    assert!(
        app.status.contains("no_role_token") || app.status.contains("failed ("),
        "the check did not run through the production path: {}",
        app.status
    );

    // Reopen the same configuration: the outcome, not the configured default, is the
    // result the Forgejo screen draws.
    let mut reopened = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("role-reopen-runner"),
    );
    reopened.show_acknowledgment = false;
    reopened.show(Screen::Forgejo);
    let screen = render_at(80, 24, &reopened);
    assert!(
        screen.contains("last check: failed (no_role_token)"),
        "the reopened role line is not the check's outcome:\n{screen}"
    );

    // A failed check followed by an address change is history for the old address;
    // it must never be printed as a role for the new one.
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Forgejo);
    app.on_key(KeyEvent::from(KeyCode::Char('c')));
    app.on_key(KeyEvent::from(KeyCode::Char('u')));
    for ch in "http://127.0.0.1:19240".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert!(
        app.url_prompt.is_none(),
        "the address was not written: {}",
        app.status
    );
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("last check: member"),
        "the old check is printed as a result for the new address:\n{screen}"
    );
    assert!(
        screen.contains("not checked this session"),
        "the address change left no honest absence:\n{screen}"
    );
}

// B3 — Runs selects the record that just became terminal, even though its id did not
// change when the run ended, and draws that run's reply.
#[test]
fn runs_moves_to_the_newest_run_when_it_ends_and_draws_its_reply() {
    let root = scratch("run-end-selection");
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000001-0001-1",
        "the older reply",
    );
    let runner = runner(
        "run-end-runner",
        "#!/bin/sh\n\
         dir=\"$2\"\n\
         id=$(basename \"$dir\")\n\
         printf '%s\\n' \"{\\\"event\\\":\\\"model_reply\\\",\\\"ts\\\":\\\"2026-10-19T00:00:00Z\\\",\\\"run_id\\\":\\\"$id\\\",\\\"turn\\\":1,\\\"status\\\":200,\\\"content\\\":\\\"the run that just ended\\\",\\\"tool_calls\\\":[],\\\"finish_reason\\\":\\\"stop\\\",\\\"usage\\\":null,\\\"body\\\":\\\"{}\\\"}\" >> \"$dir/transcript.jsonl\"\n\
         sleep 1\n\
         exit 0\n",
    );
    let mut app = app_with_runner(&root, ONE_AGENT, &runner);
    write_instruction(&app, "ledgerlite-changelog", "work\n");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "ledgerlite-changelog",
        "sk-test-key",
    )
    .expect("the key is recorded");
    app.request_agent_start();
    assert_eq!(app.supervisor.running_count(), 1, "{}", app.status);
    app.reload();
    app.show(Screen::Runs);
    let newest = app
        .recent
        .first()
        .expect("the running record is listed")
        .run_id
        .clone();
    assert_ne!(newest, "run-1700000000001-0001-1");
    app.on_key(KeyEvent::from(KeyCode::Down));
    assert_eq!(app.run_selected, 1, "the older run was not selected");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = render_at(80, 24, &app);
        app.tick();
        if app.recent.first().is_some_and(|run| run.outcome.is_some()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not become terminal: {}",
            app.status
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    assert_eq!(
        app.run_selected, 0,
        "the selection stayed on the older run: {}",
        app.status
    );
    assert_eq!(
        app.selected_run().map(|run| run.run_id.as_str()),
        Some(newest.as_str())
    );
    assert!(
        app.status.contains("view moved"),
        "the status does not say the view moved: {}",
        app.status
    );
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("the run that just ended"),
        "the newest run's reply is not drawn:\n{screen}"
    );
    let _ = app.shutdown();
}

// B4 — a zero-repository check carries no range.
#[test]
fn a_zero_repository_forgejo_title_has_no_range() {
    let root = scratch("forgejo-zero");
    let mut app = app_with(&root, ONE_AGENT);
    app.role_check = Some(Ok(empty_role_check()));
    app.show(Screen::Forgejo);
    let screen = render_at(72, 18, &app);
    assert!(
        screen.contains("no repositories"),
        "the empty list has no truthful title:\n{screen}"
    );
    assert!(
        !screen.contains("1–0 of 0"),
        "the empty list still claims a range:\n{screen}"
    );
}

// B5 — PageDown/PageUp are one viewport of the panel on every screen, including the
// Runs records panel, and never move the run selection.
#[test]
fn page_keys_scroll_the_runs_records_and_another_screen_without_moving_lists() {
    let root = scratch("paging-rule");
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000001-0001-1",
        &(0..30)
            .map(|at| format!("older line {at}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    write_run(
        &root,
        "ledgerlite-changelog",
        "run-1700000000002-0002-1",
        &(0..30)
            .map(|at| format!("newer line {at}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let mut app = app_with(&root, ONE_AGENT);
    app.reload();
    app.show(Screen::Runs);
    let _ = render_at(80, 24, &app);
    let selected = app.run_selected;
    app.on_key(KeyEvent::from(KeyCode::PageDown));
    assert!(
        app.panel_scroll > 0,
        "PageDown did not move the records panel: {}",
        app.status
    );
    assert_eq!(
        app.run_selected, selected,
        "PageDown moved the run selection instead of the records panel"
    );
    app.on_key(KeyEvent::from(KeyCode::PageUp));
    assert_eq!(app.panel_scroll, 0, "PageUp did not move the panel back");
    app.on_key(KeyEvent::from(KeyCode::Char(']')));
    assert!(app.panel_scroll > 0, "] did not move the records panel");
    app.on_key(KeyEvent::from(KeyCode::Char('[')));
    assert_eq!(app.panel_scroll, 0, "[ did not move the panel back");

    // One other screen: the Instruction preview is a panel and the same rule holds.
    write_instruction(
        &app,
        "ledgerlite-changelog",
        &(0..40)
            .map(|at| format!("instruction line {at}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    app.show(Screen::Instructions);
    let _ = render_at(80, 24, &app);
    app.on_key(KeyEvent::from(KeyCode::PageDown));
    assert!(
        app.panel_scroll > 0,
        "PageDown did not move the Instruction panel: {}",
        app.status
    );
    app.on_key(KeyEvent::from(KeyCode::PageUp));
    assert_eq!(app.panel_scroll, 0);
}

// B6 — the endpoint editor at 72×18 keeps the cursor row, the refusal row and the
// pinned action inside the border, and puts the reason directly under the URL row.
#[test]
fn the_endpoint_editor_shows_its_refusal_and_action_at_the_floor() {
    let root = scratch("editor-floor");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::AgentConfig);
    app.on_key(KeyEvent::from(KeyCode::Char('E')));
    for ch in "bad".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(ch)));
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(
        app.edit_agent
            .as_ref()
            .is_some_and(|form| form.error.is_some()),
        "the editor did not refuse the URL: {}",
        app.status
    );
    let screen = render_at(72, 18, &app);
    assert!(
        screen.contains("not written"),
        "the refusal is not drawn inside the box:\n{screen}"
    );
    assert!(
        screen.contains("ctrl-s writes it"),
        "the pinned action line is missing:\n{screen}"
    );
    let lines: Vec<&str> = screen.lines().collect();
    let url_row = lines
        .iter()
        .position(|line| line.contains("model endpoint"))
        .unwrap_or_else(|| panic!("no URL row:\n{screen}"));
    let reason_row = lines
        .iter()
        .position(|line| line.contains("not written"))
        .unwrap_or_else(|| panic!("no refusal row:\n{screen}"));
    assert!(
        reason_row > url_row && reason_row <= url_row + 2,
        "the refusal is not directly under the URL row:\n{screen}"
    );
    assert!(
        lines[url_row].contains('>'),
        "the cursor row is not drawn as the active row:\n{screen}"
    );
}

// B7 — a push is a repository fact, not a word in a command.
#[test]
fn a_command_that_only_says_git_push_offers_no_pull_request_step() {
    let root = scratch("push-negative");
    let layout = Layout::rooted_at(root.to_path_buf());
    let dir = layout
        .state_root()
        .join("ledgerlite-changelog")
        .join("run-1700000000003-0003-1");
    fs::create_dir_all(&dir).expect("the run directory");
    let mut transcript = Transcript::open(&dir).expect("a transcript");
    transcript
        .append(&Record::run_started(
            "run-1700000000003-0003-1",
            "ledgerlite-changelog",
            4321,
            "/w",
        ))
        .expect("a run starts");
    transcript
        .append(&Record::tool_call(
            Some(1),
            1,
            "shell",
            "echo git push",
            "/w",
        ))
        .expect("a tool call is recorded");
    transcript
        .append(&Record::tool_result(
            Some(1),
            1,
            "shell",
            Some(0),
            None,
            10,
            Some("git push".to_owned()),
        ))
        .expect("a tool result is recorded");
    transcript
        .append(&Record::model_reply(
            "run-1700000000003-0003-1",
            2,
            200,
            Some("nothing was pushed".to_owned()),
            Vec::new(),
            Some("stop".to_owned()),
            None,
            "{}",
        ))
        .expect("a reply is recorded");
    transcript
        .append(&Record::run_finished(
            "run-1700000000003-0003-1",
            RunOutcome::Succeeded,
            Some(0),
            1200,
            None,
        ))
        .expect("the run ends");

    let mut app = app_with(&root, ONE_AGENT);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        !screen.contains("pushed its branch"),
        "command text was read as a push:\n{screen}"
    );
    assert!(
        !screen.contains("press P"),
        "the false next step is still offered:\n{screen}"
    );
    assert!(
        screen.contains("nothing was pushed"),
        "the run is not shown:\n{screen}"
    );
}

#[test]
fn a_real_push_to_a_loopback_bare_remote_offers_the_pull_request_step() {
    let root = scratch("push-positive");
    let layout = Layout::rooted_at(root.to_path_buf());
    let agent = "ledgerlite-changelog";
    let paths = layout.agent(agent).expect("the agent path");
    let workspace = paths.workspace().to_path_buf();
    fs::create_dir_all(workspace.parent().expect("a parent")).expect("the agent directory");
    let remote = root.join("remote.git");
    git(&root, &["init", "-q", "--bare", "remote.git"]);
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            workspace.to_str().expect("utf-8"),
        ],
    );
    git(
        &workspace,
        &["config", "user.email", "agent@example.invalid"],
    );
    git(&workspace, &["config", "user.name", "Agent"]);
    fs::write(workspace.join("README.md"), "hello\n").expect("a file");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-qm", "first"]);
    git(&workspace, &["push", "-q", "-u", "origin", "HEAD"]);
    let branch = git(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let branch = branch.trim();
    assert!(!branch.is_empty());

    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(paths.instruction(), "work\n").expect("the instruction");
    gewu_multiverse::credentials::record_secret(
        &layout.credentials_file(),
        Some("model_keys"),
        agent,
        "sk-test-key",
    )
    .expect("the key is recorded");

    let run_id = "run-1700000000004-0004-1";
    let run_dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory");
    let plan = RunPlan {
        run_id: run_id.to_owned(),
        agent: agent.to_owned(),
        workspace: workspace.clone(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "stub-model".to_owned(),
        credentials: layout.credentials_file(),
        max_turns: 1,
        steps: vec!["git commit --allow-empty -qm pushed-by-run && git push".to_owned()],
        timeout_secs: 20,
    };
    plan.write(&run_dir).expect("the plan is written");
    // The model call may succeed or fail depending on what, if anything, listens on
    // the loopback port; the guard is about the repository fact written after the
    // declared step, so the call's result is not asserted.
    let _end = gewu_multiverse::agent_run::execute(&run_dir);

    let witness = gewu_multiverse::run::read_push_witness(&run_dir)
        .expect("the repository state proves the push");
    assert_eq!(witness.branch, branch);
    let recent =
        gewu_multiverse::run::recent_runs(layout.state_root(), &[agent.to_owned()], 50);
    assert!(
        recent.iter().any(gewu_multiverse::run::run_pushed),
        "the run with a real push has no witness"
    );

    let mut app = app_with(&root, ONE_AGENT);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("pushed its branch") && screen.contains("pull request"),
        "the real push does not offer the next step:\n{screen}"
    );
}

/// A bare loopback remote and a clone of it with one pushed commit.
///
/// Returns the scratch root, the remote, the clone (the run's workspace) and the
/// branch. The remote is a directory, so no network or provider is involved.
fn remote_and_clone(tag: &str) -> (PathBuf, PathBuf, PathBuf, String) {
    let root = scratch(tag);
    let remote = root.join("remote.git");
    git(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let workspace = root.join("workspace");
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            workspace.to_str().expect("utf-8"),
        ],
    );
    git(
        &workspace,
        &["config", "user.email", "agent@example.invalid"],
    );
    git(&workspace, &["config", "user.name", "Agent"]);
    fs::write(workspace.join("README.md"), "hello\n").expect("a file");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-qm", "first"]);
    git(&workspace, &["push", "-q", "-u", "origin", "HEAD"]);
    let branch = git(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]);
    (root, remote, workspace, branch.trim().to_owned())
}

/// Write one plan whose only steps are `steps`, execute it, and return the run dir.
///
/// The model endpoint is a closed loopback port, so the model call fails after the
/// declared steps; the witness is about what those steps did to the repository.
fn run_steps(root: &Path, workspace: &Path, run_id: &str, steps: Vec<String>) -> PathBuf {
    let layout = Layout::rooted_at(root.to_path_buf());
    let agent = "ledgerlite-changelog";
    let paths = layout.agent(agent).expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(paths.instruction(), "work\n").expect("the instruction");
    gewu_multiverse::credentials::record_secret(
        &layout.credentials_file(),
        Some("model_keys"),
        agent,
        "sk-test-key",
    )
    .expect("the key is recorded");
    let run_dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory");
    let plan = RunPlan {
        run_id: run_id.to_owned(),
        agent: agent.to_owned(),
        workspace: workspace.to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "stub-model".to_owned(),
        credentials: layout.credentials_file(),
        max_turns: 1,
        steps,
        timeout_secs: 20,
    };
    plan.write(&run_dir).expect("the plan is written");
    let _end = gewu_multiverse::agent_run::execute(&run_dir);
    run_dir
}

/// The repository state must produce no witness and the Runs screen no `P` offer.
fn assert_no_push_claim(root: &Path, run_dir: &Path) {
    assert!(
        gewu_multiverse::run::read_push_witness(run_dir).is_none(),
        "a push was claimed without a reflog entry that proves one"
    );
    let mut app = app_with(root, ONE_AGENT);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        !screen.contains("pushed its branch"),
        "a false push sentence is drawn:\n{screen}"
    );
    assert!(
        !screen.contains("press P"),
        "the false next step is offered:\n{screen}"
    );
}

/// The repository state must produce a witness and the Runs screen must offer `P`.
fn assert_push_claim(root: &Path, run_dir: &Path, branch: &str) {
    let witness = gewu_multiverse::run::read_push_witness(run_dir)
        .expect("the repository state proves the push");
    assert_eq!(witness.branch, branch);
    let mut app = app_with(root, ONE_AGENT);
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("pushed its branch"),
        "the real push has no next-step sentence:\n{screen}"
    );
    assert!(
        screen.contains("press P"),
        "the real push does not offer P:\n{screen}"
    );
}

/// A workspace whose remote-tracking ref a fetch can land on the local head.
///
/// The local branch has a commit the remote does not have yet. Another clone fetches
/// it from the workspace and publishes it to the remote, leaving the workspace's own
/// remote-tracking ref behind.
fn fetch_lands_fixture(tag: &str) -> (PathBuf, PathBuf, String) {
    let (root, remote, workspace, branch) = remote_and_clone(tag);
    git(
        &workspace,
        &["commit", "-q", "--allow-empty", "-m", "local commit"],
    );
    let other = root.join("other");
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            other.to_str().expect("utf-8"),
        ],
    );
    git(&other, &["config", "user.email", "other@example.invalid"]);
    git(&other, &["config", "user.name", "Other"]);
    git(
        &other,
        &["remote", "add", "w", workspace.to_str().expect("utf-8")],
    );
    git(&other, &["fetch", "-q", "w"]);
    let refspec = format!("refs/remotes/w/{branch}:refs/heads/{branch}");
    git(&other, &["push", "-q", "origin", &refspec]);
    (root, workspace, branch)
}

// B7-attack — a pull or fetch that lands the local head is not a push.
#[test]
fn a_pull_that_lands_the_local_head_is_not_a_push() {
    let (root, remote, workspace, branch) = remote_and_clone("push-pull-attack");
    // Another clone advances the remote, so the workspace's pull fast-forwards onto a
    // commit the workspace never pushed. The remote-tracking ref then equals the local
    // branch, the shape object equality alone cannot tell from a push.
    let other = root.join("other");
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            other.to_str().expect("utf-8"),
        ],
    );
    git(&other, &["config", "user.email", "other@example.invalid"]);
    git(&other, &["config", "user.name", "Other"]);
    fs::write(other.join("remote.txt"), "remote work\n").expect("a file");
    git(&other, &["add", "remote.txt"]);
    git(&other, &["commit", "-qm", "remote commit"]);
    git(&other, &["push", "-q", "origin", branch.as_str()]);

    let run_dir = run_steps(
        &root,
        &workspace,
        "run-1700000000005-0005-1",
        vec![format!("git pull -q --ff-only origin {branch}")],
    );
    let head = format!("refs/heads/{branch}");
    let tracking = format!("refs/remotes/origin/{branch}");
    let local = git(&workspace, &["rev-parse", &head]);
    let tracked = git(&workspace, &["rev-parse", &tracking]);
    assert_eq!(
        local, tracked,
        "the fixture's pull did not land the local head"
    );
    assert_no_push_claim(&root, &run_dir);
}

#[test]
fn a_fetch_that_lands_the_local_head_is_not_a_push() {
    let (root, workspace, branch) = fetch_lands_fixture("push-fetch-attack");

    let run_dir = run_steps(
        &root,
        &workspace,
        "run-1700000000006-0006-1",
        vec!["git fetch -q origin".to_owned()],
    );
    let tracking = format!("refs/remotes/origin/{branch}");
    let head = format!("refs/heads/{branch}");
    let local = git(&workspace, &["rev-parse", &head]);
    let tracked = git(&workspace, &["rev-parse", &tracking]);
    assert_eq!(
        local, tracked,
        "the fixture's fetch did not land the local head"
    );
    assert_no_push_claim(&root, &run_dir);
}

#[test]
fn a_fetch_labeled_update_by_push_is_not_a_push() {
    let (root, workspace, branch) = fetch_lands_fixture("push-relog-action");
    let run_dir = run_steps(
        &root,
        &workspace,
        "run-1700000000008-0008-1",
        vec!["GIT_REFLOG_ACTION=\"update by push\" git fetch -q origin".to_owned()],
    );
    let tracking = format!("refs/remotes/origin/{branch}");
    let head = format!("refs/heads/{branch}");
    let local = git(&workspace, &["rev-parse", &head]);
    let tracked = git(&workspace, &["rev-parse", &tracking]);
    assert_eq!(
        local, tracked,
        "the fixture's labeled fetch did not land the local head"
    );
    assert_no_push_claim(&root, &run_dir);
}

#[test]
fn a_pull_then_push_in_one_run_is_a_push() {
    let (root, remote, workspace, branch) = remote_and_clone("push-after-pull");
    let other = root.join("other");
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            other.to_str().expect("utf-8"),
        ],
    );
    git(&other, &["config", "user.email", "other@example.invalid"]);
    git(&other, &["config", "user.name", "Other"]);
    fs::write(other.join("remote.txt"), "remote work\n").expect("a file");
    git(&other, &["add", "remote.txt"]);
    git(&other, &["commit", "-qm", "remote commit"]);
    git(&other, &["push", "-q", "origin", branch.as_str()]);

    let run_dir = run_steps(
        &root,
        &workspace,
        "run-1700000000009-0009-1",
        vec![
            format!("git pull -q --ff-only origin {branch}"),
            format!("git commit --allow-empty -qm after-pull && git push -q origin {branch}"),
        ],
    );
    assert_push_claim(&root, &run_dir, &branch);
}

#[test]
fn a_second_push_in_one_run_is_a_push() {
    let (root, _remote, workspace, branch) = remote_and_clone("push-twice");
    let run_dir = run_steps(
        &root,
        &workspace,
        "run-1700000000010-0010-1",
        vec![
            format!("git commit --allow-empty -qm first-push && git push -q origin {branch}"),
            format!("git commit --allow-empty -qm second-push && git push -q origin {branch}"),
        ],
    );
    assert_push_claim(&root, &run_dir, &branch);
}

#[test]
fn a_push_with_no_reflog_makes_no_claim() {
    let (root, _remote, workspace, branch) = remote_and_clone("push-no-reflog");
    // The run really pushes, then removes the reflog that alone could prove the
    // update was a push. With no reflog the product must make no claim.
    let log = format!(".git/logs/refs/remotes/origin/{branch}");
    let command = format!(
        "git commit --allow-empty -qm pushed-by-run && git push -q origin {branch} && rm -f {log}"
    );
    let run_dir = run_steps(&root, &workspace, "run-1700000000007-0007-1", vec![command]);
    let head = format!("refs/heads/{branch}");
    let tracking = format!("refs/remotes/origin/{branch}");
    let local = git(&workspace, &["rev-parse", &head]);
    let tracked = git(&workspace, &["rev-parse", &tracking]);
    assert_eq!(
        local, tracked,
        "the fixture's push did not move the tracking ref"
    );
    assert!(
        !workspace.join(&log).exists(),
        "the reflog was not removed, so this is not the absent-reflog case"
    );
    assert_no_push_claim(&root, &run_dir);
}
