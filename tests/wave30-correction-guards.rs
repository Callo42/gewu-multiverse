//! Guards for the open pull request the product knows, the text a proposal comes
//! from, one correction per endpoint failure, the removal inventory, the Forgejo
//! floor state and consent that is recorded only after the terms were seen.
//!
//! Every guard drives the product's own `App`, key handling, model client, Forgejo
//! client and renderer. The synthetic HTTP stub is loopback-only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{
    App, EndpointFailure, EndpointTest, Screen,
};
use gewu_multiverse::config::Role;
use gewu_multiverse::credentials::Secret;
use gewu_multiverse::forgejo::{Permissions, Repository, RoleCheck, Viewer};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunOutcome, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const ONE_AGENT: &str = r#"
[forgejo]
url = "http://127.0.0.1:9"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "scribe"
model = "lab-chat-1"
endpoint = "http://127.0.0.1:9/v1/chat/completions"
instruction = "agents/scribe/instruction.md"
concurrency = 2
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-c30-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn runner(tag: &str, body: &str) -> PathBuf {
    let path = scratch(tag).join("runner.sh");
    fs::write(&path, body).expect("the runner is written");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

fn app_with_runner(root: &Path, config: &str, runner_path: &Path) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        runner_path.to_path_buf(),
    );
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
    app.show_acknowledgment = false;
    app
}

fn app_with(root: &Path, config: &str) -> App {
    app_with_runner(root, config, &runner("runner", "#!/bin/sh\nexit 0\n"))
}

/// An app on a fresh configuration with no risk record: the first-run overlay is up.
fn app_unacked(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        runner("unacked-runner", "#!/bin/sh\nexit 0\n"),
    );
    app.reload();
    assert!(app.show_acknowledgment);
    app
}

fn config_for(url: &str) -> String {
    format!(
        r#"
[forgejo]
url = "{url}"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "scribe"
model = "lab-chat-1"
endpoint = "http://127.0.0.1:9/v1/chat/completions"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#
    )
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

fn normalised(text: &str) -> String {
    text.replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

/// Clone the selected repository where `g` puts it, on a pushed `feature` branch
/// with one commit beyond `main`.
fn clone_fixture(root: &Path, agent: &str, full_name: &str) -> PathBuf {
    let remote = root.join("remote.git");
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let layout = Layout::rooted_at(root.to_path_buf());
    let paths = layout.agent(agent).expect("the agent path");
    let workspace = paths.workspace().to_path_buf();
    fs::create_dir_all(&workspace).expect("the agent workspace");
    let name = full_name.rsplit('/').next().unwrap_or(full_name);
    let destination = workspace.join(name);
    git(
        root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            destination.to_str().expect("utf-8"),
        ],
    );
    git(
        &destination,
        &["config", "user.email", "agent@example.invalid"],
    );
    git(&destination, &["config", "user.name", "Agent"]);
    fs::write(destination.join("README.md"), "hello\n").expect("a file");
    git(&destination, &["add", "README.md"]);
    git(&destination, &["commit", "-qm", "first"]);
    git(&destination, &["push", "-q", "-u", "origin", "HEAD"]);
    git(&destination, &["checkout", "-q", "-b", "feature"]);
    git(&destination, &["commit", "-q", "--allow-empty", "-m", "work"]);
    git(&destination, &["push", "-q", "-u", "origin", "feature"]);
    destination
}

fn repo(full_name: &str) -> Repository {
    Repository {
        full_name: full_name.to_owned(),
        namespace: full_name
            .split_once('/')
            .map(|(ns, _)| ns.to_owned())
            .unwrap_or_default(),
        name: full_name
            .split_once('/')
            .map(|(_, name)| name.to_owned())
            .unwrap_or_else(|| full_name.to_owned()),
        private: false,
        default_branch: Some("main".to_owned()),
        html_url: None,
        permissions: Permissions {
            admin: false,
            pull: true,
            push: true,
        },
    }
}

fn role_check(full_name: &str, url: &str) -> RoleCheck {
    RoleCheck {
        url: url.to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "ken".to_owned(),
            email: None,
            description: None,
            is_admin: false,
        },
        repositories: vec![repo(full_name)],
        private_visible: 0,
    }
}

fn role_check_replies(default_branch: &str) -> Vec<StubReply> {
    vec![
        StubReply::json(r#"{"login":"ken","email":"ken@example.invalid","is_admin":false}"#),
        StubReply::json(format!(
            r#"{{"data":[{{"full_name":"lab/analysis-code","name":"analysis-code","private":false,"default_branch":"{default_branch}","html_url":null,"permissions":{{"admin":false,"push":true,"pull":true}}}}]}}"#
        )),
    ]
}

fn record_identity(app: &App, agent: &str) {
    gewu_multiverse::credentials::record_agent_identity(
        &app.credentials_path,
        agent,
        Some(&Secret::new("synthetic-agent-token")),
        "ken",
        "ken@example.invalid",
        "supplied",
        None,
    )
    .expect("the identity is recorded");
}

fn write_run(root: &Path, agent: &str, run_id: &str, reply: &str) -> PathBuf {
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
    dir
}

fn write_plan(root: &Path, run_dir: &Path, run_id: &str) {
    let layout = Layout::rooted_at(root.to_path_buf());
    let paths = layout.agent("scribe").expect("the agent path");
    let plan = RunPlan {
        run_id: run_id.to_owned(),
        agent: "scribe".to_owned(),
        workspace: paths.workspace().to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "lab-chat-1".to_owned(),
        credentials: layout.credentials_file(),
        max_turns: 1,
        steps: Vec::new(),
        timeout_secs: 20,
    };
    plan.write(run_dir).expect("the plan is written");
}

fn publish_witness(run_dir: &Path, branch: &str) {
    gewu_multiverse::run::write_push_witness(run_dir, branch, &format!("refs/remotes/origin/{branch}"))
        .expect("the push witness is written");
}

fn forgejo_ready_app(root: &Path, stub: &Stub, clone: &Path) -> App {
    let mut app = app_with(root, &config_for(&format!("http://{}", stub.addr())));
    let _ = clone;
    record_identity(&app, "scribe");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);
    app
}

fn open_pr(number: u64, title: &str, head: &str, base: &str, login: &str) -> String {
    format!(
        r#"{{"number":{number},"title":"{title}","state":"open","head":{{"ref":"{head}","repo":{{"full_name":"lab/analysis-code"}}}},"base":{{"ref":"{base}","repo":{{"full_name":"lab/analysis-code"}}}},"user":{{"login":"{login}"}}}}"#
    )
}

// 1 — the product knows the open pull request, and U acts only on this branch's.

#[test]
fn p_writes_nothing_when_this_branch_already_has_an_open_request() {
    let list = format!(
        "[{}, {}, {}]",
        open_pr(1, "Proofread the guide", "feature", "main", "ken"),
        open_pr(2, "Another branch", "other", "main", "ken"),
        open_pr(3, "Another base", "feature", "release", "someone-else"),
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("p-existing");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "a proposal that must not be shown",
    );

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("#1")),
        "the refusal does not name the open number: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("Proofread the guide")),
        "the refusal does not name the open title: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("feature -> main")),
        "the refusal does not name the branch pair: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("U")),
        "the refusal does not say how to update it: {}",
        app.status
    );
    assert!(
        app.confirm_pull_request.is_none(),
        "a preview was shown for an already-open request"
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "one list call was expected: {requests:?}");
    assert_eq!(requests[0].method, "GET", "{requests:?}");
    assert!(
        requests[0].path.contains("/pulls?state=open"),
        "the lookup did not use the list call: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.method != "POST"),
        "P wrote a pull request: {requests:?}"
    );

    let screen = normalised(&render_at(80, 24, &app));
    assert!(screen.contains("#1"), "the screen does not know the request: {screen}");
    assert!(screen.contains("Proofread the guide"), "{screen}");

    // The floor keeps the selected repository row and the open-request fact together.
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains("> lab/analysis-code"),
        "the selected repository left the floor when the request was known:\n{floor}"
    );
    assert!(
        normalised(&floor).contains("#1"),
        "the floor does not state the open request:\n{floor}"
    );
}

#[test]
fn u_previews_the_matching_request_and_writes_only_on_yes() {
    let list = format!(
        "[{}, {}, {}, {}]",
        open_pr(10, "Another account, same branch", "feature", "main", "someone-else"),
        open_pr(1, "Proofread the guide", "feature", "main", "ken"),
        open_pr(2, "Another branch", "other", "main", "ken"),
        open_pr(3, "Another base", "feature", "release", "ken"),
    );
    let patch = StubReply::json(
        r#"{"number":1,"title":"Proofread the guide","state":"open","head":{"ref":"feature"},"base":{"ref":"main"}}"#,
    );
    let stub = Stub::start(vec![StubReply::json(list.clone()), StubReply::json(list), patch]);
    let root = scratch("u-preview");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let run_dir = write_run(&root, "scribe", "run-1700000000002-0002-1", "Proofread the guide\n\nbody");
    publish_witness(&run_dir, "feature");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("press y or enter")),
        "U wrote without a preview: {}",
        app.status
    );
    assert_eq!(stub.requests().len(), 1, "U must list before it previews");
    assert_eq!(stub.requests()[0].method, "GET");
    let preview = normalised(&render_at(100, 30, &app));
    assert!(preview.contains("#1"), "the preview has no number: {preview}");
    assert!(
        preview.contains(&normalised("feature -> main")),
        "the preview has no branch pair: {preview}"
    );
    assert!(
        preview.contains(&normalised("Proofread the guide")),
        "the preview has no title: {preview}"
    );

    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert_eq!(stub.requests().len(), 1, "n wrote a pull request");
    assert!(
        app.status.contains("nothing was written"),
        "{}",
        app.status
    );

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let requests = stub.requests();
    assert_eq!(requests.len(), 3, "one list and one update expected: {requests:?}");
    let patched: Vec<_> = requests.iter().skip(2).collect();
    assert_eq!(patched.len(), 1, "{requests:?}");
    assert_eq!(patched[0].method, "PATCH", "{requests:?}");
    assert!(
        patched[0].path.ends_with("/pulls/1"),
        "U patched the wrong request: {requests:?}"
    );
    assert!(app.status.contains("#1"), "{}", app.status);
}

#[test]
fn u_refuses_when_no_request_heads_this_branch() {
    let list = format!(
        "[{}, {}]",
        open_pr(2, "Another branch", "other", "main", "ken"),
        open_pr(3, "Another base", "feature", "release", "ken"),
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("u-none");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("feature")),
        "the refusal does not name the branch: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("no open pull request")),
        "{}",
        app.status
    );
    assert!(
        status.contains(&normalised("P")),
        "the refusal does not say P opens one: {}",
        app.status
    );
    assert!(
        stub.requests().iter().all(|request| request.method != "PATCH"),
        "U wrote with nothing to update: {:?}",
        stub.requests()
    );
}

#[test]
fn p_refuses_when_the_open_requests_cannot_be_read() {
    let stub = Stub::start(vec![StubReply::status(
        500,
        r#"{"message":"the pull list broke","type":"server_error"}"#,
    )]);
    let root = scratch("p-list-fail");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("could not be listed")),
        "the refusal does not name the cause: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("server_error")),
        "the refusal does not name the class: {}",
        app.status
    );
    assert!(!app.status.contains('{'), "the raw reply is on screen: {}", app.status);
    assert!(
        app.confirm_pull_request.is_none(),
        "a preview was shown without a list"
    );
    assert!(
        stub.requests().iter().all(|request| request.method != "POST"),
        "P wrote after a failed lookup: {:?}",
        stub.requests()
    );
}

#[test]
fn a_conflict_on_create_is_one_sentence_naming_the_conflict() {
    let stub = Stub::start(vec![
        StubReply::json("[]"),
        StubReply::status(
            409,
            r#"{"message":"pull request already exists for feature","url":"http://example.invalid"}"#,
        ),
    ]);
    let root = scratch("p-409");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    write_run(&root, "scribe", "run-1700000000002-0002-1", "Open this\n\nbody");

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("409")) || status.contains(&normalised("conflict")),
        "the conflict is not named: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("already")),
        "the sentence does not say what conflicted: {}",
        app.status
    );
    assert!(
        !app.status.contains('{') && !app.status.contains("http://example.invalid"),
        "the raw reply body is on screen: {}",
        app.status
    );
}

#[test]
fn a_role_check_lists_the_selected_branchs_open_request() {
    let mut replies = role_check_replies("main");
    replies.push(StubReply::json(format!(
        "[{}]",
        open_pr(7, "Proofread the guide", "feature", "main", "ken")
    )));
    let stub = Stub::start(replies);
    let root = scratch("c-lookup");
    let config = config_for(&format!("http://{}", stub.addr()));
    let mut app = app_with(&root, &config);
    clone_fixture(&root, "scribe", "lab/analysis-code");
    record_identity(&app, "scribe");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        None,
        "forgejo_token",
        "synthetic-role-token",
    )
    .expect("the role token is written");
    app.reload();
    app.show(Screen::Forgejo);

    let before = normalised(&render_at(80, 24, &app));
    assert!(
        before.contains(&normalised("not checked this session")),
        "{before}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('c')));
    app.wait_for_probe();
    let after = normalised(&render_at(80, 24, &app));
    assert!(
        after.contains(&normalised("#7")),
        "the screen does not know the open request: {after}"
    );
    assert!(after.contains("Proofread the guide"), "{after}");
}

#[test]
fn the_runs_note_names_an_open_request_after_a_witnessed_push() {
    let list = format!(
        "[{}]",
        open_pr(1, "Proofread the guide", "feature", "main", "ken")
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("runs-open-pr");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "Proofread the guide\n\nbody",
    );
    publish_witness(&run_dir, "feature");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    // Make the session learn the open request, then read the run note.
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.show(Screen::Runs);
    let screen = normalised(&render_at(120, 40, &app));
    assert!(
        screen.contains(&normalised("#1")),
        "the Runs note does not name the open request: {screen}"
    );
    assert!(
        screen.contains(&normalised("update")),
        "the Runs note does not name the update route: {screen}"
    );
}

// 2 — the proposal's text comes from the run that pushed the branch.

#[test]
fn the_proposal_text_comes_from_the_run_that_pushed_this_branch() {
    let stub = Stub::start(vec![StubReply::json("[]")]);
    let root = scratch("text-pushed-run");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let pushed = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "Proofread this week's documentation\n\nBranch: proofread/week-42",
    );
    write_plan(&root, &pushed, "run-1700000000002-0002-1");
    publish_witness(&pushed, "feature");
    write_run(
        &root,
        "scribe",
        "run-1700000000003-0003-1",
        "Nothing to proofread this week: no Markdown files changed.",
    );
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let preview = normalised(&render_at(100, 30, &app));
    assert!(
        preview.contains(&normalised("Proofread this week's documentation")),
        "the proposal is not from the pushed run: {preview}"
    );
    assert!(
        !preview.contains(&normalised("Nothing to proofread")),
        "the proposal came from a run that pushed nothing: {preview}"
    );
    assert!(
        preview.contains(&normalised("run-1700000000002-0002-1")),
        "the preview does not name the run source: {preview}"
    );
}

#[test]
fn a_present_pull_request_file_still_wins_over_a_pushed_run() {
    let stub = Stub::start(vec![StubReply::json("[]")]);
    let root = scratch("text-file");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let pushed = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "The pushed run's title\n\nbody",
    );
    publish_witness(&pushed, "feature");
    let paths = Layout::rooted_at(root.clone())
        .agent("scribe")
        .expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(
        paths.config_dir().join("pull-request.txt"),
        "The file's title\nThe file's body",
    )
    .expect("the file is written");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let preview = normalised(&render_at(100, 30, &app));
    assert!(
        preview.contains(&normalised("The file's title")),
        "the file did not win: {preview}"
    );
    assert!(
        !preview.contains(&normalised("The pushed run's title")),
        "the pushed run overrode the file: {preview}"
    );
    assert!(
        preview.contains("pull-request.txt"),
        "the preview does not name the file source: {preview}"
    );
}

#[test]
fn a_run_that_pushed_another_branch_is_not_the_source() {
    let stub = Stub::start(vec![StubReply::json("[]")]);
    let root = scratch("text-other-branch");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let feature_run = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "The feature branch's title\n\nbody",
    );
    publish_witness(&feature_run, "feature");
    let other_run = write_run(
        &root,
        "scribe",
        "run-1700000000003-0003-1",
        "The other branch's title\n\nbody",
    );
    publish_witness(&other_run, "another-branch");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let preview = normalised(&render_at(100, 30, &app));
    assert!(
        preview.contains(&normalised("The feature branch's title")),
        "the other branch's run was used: {preview}"
    );
    assert!(
        !preview.contains(&normalised("The other branch's title")),
        "a run that pushed another branch was the source: {preview}"
    );
}

#[test]
fn without_a_named_push_the_text_comes_from_the_branchs_commits() {
    let stub = Stub::start(vec![StubReply::json("[]")]);
    let root = scratch("text-commits");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    git(
        &clone,
        &["commit", "-q", "--allow-empty", "-m", "Polish the install guide"],
    );
    git(
        &clone,
        &["commit", "-q", "--allow-empty", "-m", "Fix the endpoint example"],
    );
    git(&clone, &["push", "-q", "origin", "feature"]);
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &format!("http://{}", stub.addr()))));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let preview = normalised(&render_at(100, 30, &app));
    assert!(
        preview.contains(&normalised("Fix the endpoint example")),
        "the newest subject is not the title: {preview}"
    );
    assert!(
        preview.contains(&normalised("Polish the install guide")),
        "the bounded subject list is not the body: {preview}"
    );
    assert!(
        preview.contains(&normalised("commits")),
        "the preview does not name the commit source: {preview}"
    );
}

// 3 — one correction per endpoint failure, and a stale result is one line.

fn endpoint_failure(app: &mut App, class: &str, message: &str) {
    app.endpoint_test = Some(EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "lab-chat-1".to_owned(),
        result: Err(EndpointFailure {
            class: class.to_owned(),
            message: message.to_owned(),
        }),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: None,
    });
}

fn agent_screen(app: &mut App) -> String {
    app.show(Screen::AgentConfig);
    normalised(&render_at(100, 40, app))
}

#[test]
fn timeout_and_the_other_failure_classes_each_name_one_correction() {
    let root = scratch("failures");
    let cases = [
        ("timeout", "the request timed out after 15 s", "press p"),
        ("rate_limited", "the endpoint rate-limited the request (HTTP 429)", "press p"),
        ("malformed_reply", "the reply is not a usable chat completion", "press 2 and E"),
        ("missing_key", "the credentials file records no model key for this agent", "press 5 and k"),
        ("unknown", "the stored result is not readable", "press p"),
    ];
    for (class, message, correction) in cases {
        let mut app = app_with(&root, ONE_AGENT);
        endpoint_failure(&mut app, class, message);
        let screen = agent_screen(&mut app);
        assert!(
            screen.contains(&normalised(correction)),
            "{class} names no correction `{correction}`: {screen}"
        );
    }
}

#[test]
fn timeout_does_not_send_a_person_to_start_a_run() {
    let root = scratch("timeout-no-s");
    let mut app = app_with(&root, ONE_AGENT);
    endpoint_failure(&mut app, "timeout", "the request timed out after 15 s");
    let screen = agent_screen(&mut app);
    assert!(screen.contains("press p"), "{screen}");
    assert!(
        !screen.contains("press s"),
        "timeout tells a person to start a run: {screen}"
    );
}

#[test]
fn a_server_error_shows_the_providers_message_and_points_at_the_test_key() {
    let stub = Stub::start(vec![StubReply::status(
        500,
        r#"{"error":{"message":"fixture upstream failed","type":"server_error","param":null,"code":"server_error"}}"#,
    )]);
    let root = scratch("server-message");
    let config = config_for("https://forgejo.example.org");
    let mut app = app_with(&root, &config);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-model-key",
    )
    .expect("the model key is written");
    {
        let text = fs::read_to_string(root.join("config.toml")).expect("the configuration reads");
        let text = text.replace(
            "http://127.0.0.1:9/v1/chat/completions",
            &stub.url(),
        );
        fs::write(root.join("config.toml"), text).expect("the endpoint is configured");
    }
    app.reload();
    app.on_key(KeyEvent::from(KeyCode::Char('p')));
    app.wait_for_probe();
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("fixture upstream failed")),
        "the provider's message is not shown: {}",
        app.status
    );
    assert!(
        !app.status.contains("{\"error\""),
        "the raw JSON envelope is on screen: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("press p")),
        "the correction does not name p: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("press s")),
        "the correction names s: {}",
        app.status
    );
}

#[test]
fn a_stale_endpoint_result_is_one_line_and_does_not_repeat_the_old_failure() {
    let root = scratch("stale-one-line");
    let mut app = app_with(
        &root,
        &ONE_AGENT.replace("lab-chat-1", "lab-chat-2"),
    );
    app.endpoint_test = Some(EndpointTest {
        agent: "scribe".to_owned(),
        endpoint: "http://127.0.0.1:8/old/chat/completions".to_owned(),
        model: "old-model-9".to_owned(),
        result: Err(EndpointFailure {
            class: "model_not_found".to_owned(),
            message: "the endpoint's models list does not include the model \"old-model-9\"".to_owned(),
        }),
        tested_at_ms: gewu_multiverse::time::now_unix_ms(),
        machine: None,
    });
    let screen = agent_screen(&mut app);
    assert!(
        screen.contains(&normalised("not current")),
        "the stale result is not marked: {screen}"
    );
    assert!(
        screen.contains(&normalised("previous settings")),
        "the stale line does not say what changed: {screen}"
    );
    assert!(
        screen.contains(&normalised("press p")),
        "the stale line does not name the testing key: {screen}"
    );
    assert!(
        !screen.contains("old-model-9"),
        "the old model id is repeated at length: {screen}"
    );
}

// 4 — the removal confirmation lists what will actually remain.

#[test]
fn the_removal_confirmation_names_only_what_exists() {
    let root = scratch("remove-empty");
    let mut app = app_with(&root, ONE_AGENT);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-model-key",
    )
    .expect("the model key is written");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("forgejo_agent_tokens"),
        "scribe",
        "synthetic-agent-token",
    )
    .expect("the agent token is written");
    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char('d')));
    let screen = normalised(&render_at(100, 40, &app));
    assert!(
        screen.contains(&normalised("model key")),
        "the remaining model key is not named: {screen}"
    );
    assert!(
        screen.contains(&normalised("Forgejo token")) || screen.contains(&normalised("agent token")),
        "the remaining agent token is not named: {screen}"
    );
    assert!(
        screen.contains(&normalised("Credentials")) && screen.contains("D"),
        "the confirmation does not name where to delete the secrets: {screen}"
    );
    assert!(
        !screen.contains(&normalised("no files under")),
        "a directory that does not exist is named: {screen}"
    );
    assert!(
        !screen.contains(&normalised("run records under")),
        "run records that do not exist are named: {screen}"
    );
}

#[test]
fn the_removal_confirmation_counts_an_existing_directory_and_run_records() {
    let root = scratch("remove-present");
    let mut app = app_with(&root, ONE_AGENT);
    let paths = Layout::rooted_at(root.clone())
        .agent("scribe")
        .expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::write(paths.config_dir().join("instruction.md"), "do the work\n")
        .expect("the instruction is written");
    for run in ["run-1", "run-2", "run-3"] {
        fs::create_dir_all(
            Layout::rooted_at(root.clone())
                .state_root()
                .join("scribe")
                .join(run),
        )
        .expect("a run directory");
    }
    app.reload();
    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char('d')));
    let screen = normalised(&render_at(100, 40, &app));
    assert!(
        screen.contains(&normalised("1 file")),
        "the existing file count is missing: {screen}"
    );
    assert!(
        screen.contains(&normalised("3 run records")),
        "the existing run-record count is missing: {screen}"
    );
    assert!(
        screen.contains("agents/scribe") || screen.contains(&normalised("scribe")),
        "the existing directory is not named: {screen}"
    );
}

#[test]
fn the_removal_status_agrees_with_the_inventory() {
    let root = scratch("remove-status");
    let mut app = app_with(&root, ONE_AGENT);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-model-key",
    )
    .expect("the model key is written");
    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char('d')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("model key")),
        "the status hides the model key the confirmation named: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("files under")),
        "the status claims files that did not exist: {}",
        app.status
    );
}

// 5 — the Forgejo screen without a Forgejo, and the floor.

#[test]
fn the_forgejo_screen_says_the_connection_is_optional() {
    let root = scratch("forgejo-optional");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        runner("forgejo-optional-runner", "#!/bin/sh\nexit 0\n"),
    );
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = normalised(&render_at(80, 24, &app));
    assert!(
        screen.contains(&normalised("Forgejo is optional")),
        "the screen never says the connection is optional: {screen}"
    );
    assert!(
        screen.contains(&normalised("without")),
        "the screen does not say agents run without one: {screen}"
    );
    assert!(
        screen.contains(&normalised("press c")),
        "the screen does not say what c does: {screen}"
    );
}

#[test]
fn the_selected_repository_is_drawn_on_entry_at_the_floor() {
    let root = scratch("forgejo-floor");
    let mut app = app_with(&root, ONE_AGENT);
    let mut check = role_check("lab/analysis-code", "https://forgejo.example.org");
    check.repositories.push(repo("lab/other-repository"));
    app.role_check = Some(Ok(check));
    app.repo_selected = 1;
    app.show(Screen::Forgejo);
    let screen = render_at(72, 18, &app);
    let selected = screen
        .lines()
        .find(|line| line.contains("lab/other-repository"))
        .unwrap_or_else(|| panic!("the selected repository row is not drawn on entry:\n{screen}"));
    assert!(
        selected.contains('>') || screen.contains(&format!("> {}", "lab/other-repository")),
        "the selected repository carries no marker:\n{screen}"
    );
}

// 6 — consent is never recorded unseen.

#[test]
fn at_the_floor_yes_records_nothing_until_every_term_was_on_screen() {
    let root = scratch("consent-floor");
    let mut app = app_unacked(&root, ONE_AGENT);
    let record = root.join("risk-acknowledgment.toml");
    assert!(!record.exists());
    let top = render_at(72, 18, &app);
    assert!(
        normalised(&top).contains(&normalised("terms remain")),
        "the answer row does not say how many terms remain:\n{top}"
    );
    assert!(
        normalised(&top).contains(&normalised("]")),
        "the answer row does not say ] shows them:\n{top}"
    );

    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(
        !record.exists(),
        "the terms were recorded before every term was on screen"
    );
    assert!(app.show_acknowledgment, "the overlay closed without recording");
    assert!(
        normalised(&app.status).contains(&normalised("terms")),
        "y is refused in silence: {}",
        app.status
    );

    for _ in 0..40 {
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
        let screen = normalised(&render_at(72, 18, &app));
        if !screen.contains(&normalised("terms remain")) {
            break;
        }
    }
    let after = normalised(&render_at(72, 18, &app));
    assert!(
        after.contains(&normalised("I understand, record it once")),
        "the answer row never became confirmable:\n{after}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(record.exists(), "the record was not written after reading");
    assert!(!app.show_acknowledgment);
}

#[test]
fn at_eighty_by_twenty_four_the_whole_box_still_confirms() {
    let root = scratch("consent-80");
    let mut app = app_unacked(&root, ONE_AGENT);
    let record = root.join("risk-acknowledgment.toml");
    let _ = render_at(80, 24, &app);
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(record.exists(), "the whole box at 80x24 did not confirm");
}

#[test]
fn no_still_acts_at_the_floor_before_the_terms_were_seen() {
    let root = scratch("consent-no");
    let mut app = app_unacked(&root, ONE_AGENT);
    let _ = render_at(72, 18, &app);
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert!(!app.show_acknowledgment);
    assert!(!app.ack_confirmed());
}

// Small copy inside the accepted classes.

#[test]
fn a_wrapped_bullet_keeps_its_hanging_indent() {
    let root = scratch("bullet-indent");
    let app = app_unacked(&root, ONE_AGENT);
    let screen = render_at(72, 18, &app);
    let continuation = screen
        .lines()
        .find(|line| line.contains("too."))
        .unwrap_or_else(|| panic!("the wrapped bullet continuation is missing:\n{screen}"));
    let after_border = continuation
        .split_once('│')
        .map(|(_, rest)| rest)
        .unwrap_or(continuation);
    assert!(
        after_border.starts_with("    "),
        "the wrapped bullet lost its hanging indent: {continuation:?}"
    );
}
