//! Guards for the pull-request gate and preview, overlay-owned footers and frames,
//! an honest repository-list state, one cause per failure, column priority,
//! whole-line panels and a quiet hub.
//!
//! Every guard drives the product's own `App`, key handling, storage, model client,
//! Forgejo client and renderer. The synthetic HTTP stubs are loopback-only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{
    ActionKind, App, PendingAction, PullRequestPreview, RoleCheckStamp, Screen, SecretTarget,
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
url = "https://forgejo.example.org"
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
    let root = std::env::temp_dir().join(format!("gewu-c29-{tag}-{}-{nanos}", std::process::id()));
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

fn bare_remote(root: &Path) -> PathBuf {
    let remote = root.join("remote.git");
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    remote
}

/// Clone the selected repository into the agent's workspace exactly where `g` puts
/// it, then move to `feature`. `ahead` adds a commit; `publish` pushes it.
fn clone_fixture(root: &Path, agent: &str, full_name: &str, ahead: bool, publish: bool) -> PathBuf {
    let remote = bare_remote(root);
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
    let workspace = destination;
    git(
        &workspace,
        &["config", "user.email", "agent@example.invalid"],
    );
    git(&workspace, &["config", "user.name", "Agent"]);
    fs::write(workspace.join("README.md"), "hello\n").expect("a file");
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-qm", "first"]);
    git(&workspace, &["push", "-q", "-u", "origin", "HEAD"]);
    git(&workspace, &["checkout", "-q", "-b", "feature"]);
    if ahead {
        git(&workspace, &["commit", "-q", "--allow-empty", "-m", "work"]);
    }
    if publish {
        git(&workspace, &["push", "-q", "-u", "origin", "feature"]);
    }
    workspace
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

/// The two replies one role check reads: the viewer, then the repository search.
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

/// A run still in flight: an opening record and a plan, but no terminal record.
fn write_running_run(root: &Path, agent: &str, run_id: &str) -> PathBuf {
    let layout = Layout::rooted_at(root.to_path_buf());
    let dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&dir).expect("the run directory");
    let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(run_id, agent, 4321, "/w"))
        .expect("a run starts");
    fs::write(dir.join(gewu_multiverse::run::LOG_FILE), "a log\n").expect("a log is written");
    dir
}

/// End a running record in place: the id does not change when the run ends.
fn finish_run(dir: &Path, run_id: &str) {
    let mut transcript = Transcript::open(dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_finished(
            run_id,
            RunOutcome::Succeeded,
            Some(0),
            1200,
            None,
        ))
        .expect("the run ends");
}

/// Write a run's plan naming the agent workspace root, as the product's own start does.
fn publish_witness(run_dir: &Path, branch: &str) {
    gewu_multiverse::run::write_push_witness(
        run_dir,
        branch,
        &format!("refs/remotes/origin/{branch}"),
    )
    .expect("the push witness is written");
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

/// Advance the bare remote's `main` from a second working copy, then fetch in `clone`.
///
/// The clone's local `main` is now behind `origin/main`: a normal fetch-without-update.
fn advance_remote_main(root: &Path, clone: &Path) {
    let remote = root.join("remote.git");
    let other = root.join("remote-advance");
    git(
        root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            other.to_str().expect("utf-8"),
        ],
    );
    git(&other, &["config", "user.email", "advance@example.invalid"]);
    git(&other, &["config", "user.name", "Advance"]);
    fs::write(other.join("advance.txt"), "advance\n").expect("the file is written");
    git(&other, &["add", "advance.txt"]);
    git(&other, &["commit", "-qm", "advance main"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(clone, &["fetch", "-q", "origin"]);
}

fn endpoint_config(url: &str, endpoint: &str) -> String {
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
endpoint = "{endpoint}"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#
    )
}

// 1 — P refuses with one reason, and the legend names it only when it can act.

#[test]
fn p_refuses_when_the_current_branch_is_the_base() {
    let root = scratch("p-base");
    let mut app = app_with(&root, ONE_AGENT);
    clone_fixture(&root, "scribe", "lab/analysis-code", false, false);
    let workspace = Layout::rooted_at(root.clone())
        .agent("scribe")
        .expect("the agent path")
        .workspace()
        .join("analysis-code");
    git(&workspace, &["checkout", "-q", "main"]);
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(app.status.contains("base branch"), "{}", app.status);
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend offers P: {screen}"
    );
}

#[test]
fn p_refuses_when_the_branch_has_no_commits_the_base_lacks() {
    let root = scratch("p-no-commits");
    let mut app = app_with(&root, ONE_AGENT);
    clone_fixture(&root, "scribe", "lab/analysis-code", false, false);
    record_identity(&app, "scribe");
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(
        app.status.contains("no commits") && app.status.contains("lacks"),
        "{}",
        app.status
    );
}

#[test]
fn p_refuses_a_witnessed_run_when_the_branch_is_not_on_the_remote() {
    let root = scratch("p-no-push");
    let mut app = app_with(&root, ONE_AGENT);
    clone_fixture(&root, "scribe", "lab/analysis-code", true, false);
    record_identity(&app, "scribe");
    // A run with a plan and a push witness, as the product writes them. The witness
    // drives the Runs note; it is not evidence that this clone's branch is on the
    // remote, so it must not make `P` pass.
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000001-0001-1",
        "nothing was pushed",
    );
    gewu_multiverse::run::write_push_witness(&run_dir, "feature", "refs/remotes/origin/feature")
        .expect("the push witness is written");
    write_plan(&root, &run_dir, "run-1700000000001-0001-1");
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(
        app.status.contains("not on the remote") && app.status.contains("push it first"),
        "{}",
        app.status
    );
    assert!(
        !app.status.contains("press y or enter"),
        "the witness reached a preview: {}",
        app.status
    );
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend offers P: {screen}"
    );
}

#[test]
fn p_shows_title_body_and_branch_pair_and_writes_only_on_y() {
    let stub = Stub::start(vec![
        StubReply::json("[]"),
        StubReply::json("[]"),
        StubReply::json(
            r#"{"number":4,"title":"lab: first work","state":"open","head":{"ref":"feature"},"base":{"ref":"main"}}"#,
        ),
    ]);
    let root = scratch("p-preview");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    // The fixture pushes `feature` for real; the clone's own remote-tracking ref is
    // the gate's evidence. No push witness is seeded.
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    assert!(
        gewu_multiverse::workspace::branch_on_remote(&clone, "feature"),
        "the fixture's real push left no remote-tracking ref"
    );
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "first line of the reply\n\nand the body",
    );
    publish_witness(&run_dir, "feature");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &stub.url())));
    app.show(Screen::Forgejo);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        normalised(&app.status).contains(&normalised("press y or enter")),
        "{}",
        app.status
    );
    let before = stub.requests();
    assert_eq!(before.len(), 1, "one list call was expected: {before:?}");
    assert_eq!(before[0].method, "GET", "{before:?}");
    assert!(
        before.iter().all(|request| request.method != "POST"),
        "a write happened before confirmation: {before:?}"
    );
    let preview = render_at(100, 30, &app);
    assert!(
        normalised(&preview).contains(&normalised("feature -> main")),
        "{preview}"
    );
    assert!(preview.contains("first line of the reply"), "{preview}");
    assert!(preview.contains("and the body"), "{preview}");
    assert!(
        normalised(&preview).contains(&normalised("y or enter opens it")),
        "{preview}"
    );
    assert!(
        normalised(&preview).contains(&normalised("writes nothing")),
        "{preview}"
    );

    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert_eq!(stub.requests().len(), 1, "n wrote a pull request");
    assert!(app.status.contains("nothing was written"), "{}", app.status);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.on_key(KeyEvent::from(KeyCode::Enter));
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        3,
        "a second list and one create request were expected: {requests:?}"
    );
    assert_eq!(requests[2].method, "POST", "{requests:?}");
    assert!(requests[2].path.ends_with("/pulls"), "{}", requests[2].path);
    let body = &requests[2].body;
    assert!(body.contains("\"head\":\"feature\""), "{body}");
    assert!(body.contains("\"base\":\"main\""), "{body}");
    assert!(body.contains("first line of the reply"), "{body}");
    assert!(app.status.contains("opened #4"), "{}", app.status);
}

#[test]
fn a_branch_pushed_by_an_earlier_run_can_be_opened_after_a_later_run_does_not_push() {
    let stub = Stub::start(vec![
        StubReply::json("[]"),
        StubReply::json(
            r#"{"number":5,"title":"lab: earlier branch","state":"open","head":{"ref":"feature"},"base":{"ref":"main"}}"#,
        ),
    ]);
    let root = scratch("p-earlier-push");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000003-0003-1",
        "the branch is already on the remote",
    );
    write_plan(&root, &run_dir, "run-1700000000003-0003-1");
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &stub.url())));
    app.show(Screen::Forgejo);
    let screen = render_at(120, 40, &app);
    assert!(
        screen.contains("P pull request"),
        "the legend withholds a usable P: {screen}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        normalised(&app.status).contains(&normalised("press y or enter")),
        "{}",
        app.status
    );
    let before = stub.requests();
    assert_eq!(before.len(), 1, "the lookup did not run: {before:?}");
    assert_eq!(before[0].method, "GET", "{before:?}");
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert_eq!(stub.requests().len(), 2, "the pull request was not opened");
}

#[test]
fn a_clone_note_keeps_both_legend_rows_with_a_populated_list() {
    let root = scratch("forgejo-legend-clone");
    let mut app = app_with(&root, ONE_AGENT);
    let mut check = role_check("lab/analysis-code", "https://forgejo.example.org");
    check.repositories.push(repo("lab/second-repository"));
    check.repositories.push(repo("lab/third-repository"));
    app.role_check = Some(Ok(check));
    app.forgejo_note = Some(
        "lab/analysis-code was cloned into the agent workspace as ken <ken@example.invalid>"
            .to_owned(),
    );
    app.show(Screen::Forgejo);
    // The populated list plus the clone note is the state the short window had to
    // scroll; the legend is pinned as two whole rows above the scrolling content.
    for (width, height) in [(80u16, 24u16), (72, 18)] {
        let screen = render_at(width, height, &app);
        let flat = normalised(&screen);
        assert!(
            flat.contains(&normalised("u url   c role   C admin role")),
            "the first key row is missing after a clone at {width}x{height}:\n{screen}"
        );
        assert!(
            flat.contains(&normalised("up/down choose")),
            "the second key row is missing after a clone at {width}x{height}:\n{screen}"
        );
        assert!(
            flat.contains(&normalised("U update this branch's request")),
            "the second row lost its object after a clone at {width}x{height}:\n{screen}"
        );
    }
}

#[test]
fn p_measures_the_base_from_the_clones_remote_tracking_ref() {
    let stub = Stub::start(Vec::new());
    let root = scratch("p-stale-base");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", false, false);
    record_identity(&app, "scribe");
    // A normal fetch-without-update: local main is behind origin/main.
    advance_remote_main(&root, &clone);
    git(&clone, &["checkout", "-q", "-b", "stale", "origin/main"]);
    git(&clone, &["push", "-q", "-u", "origin", "stale"]);
    assert_eq!(
        git(&clone, &["rev-list", "--count", "main..stale"]).trim(),
        "1",
        "the fixture's local base is not behind"
    );
    assert_eq!(
        git(&clone, &["rev-list", "--count", "origin/main..stale"]).trim(),
        "0",
        "the fixture's branch is not level with the real base"
    );
    app.role_check = Some(Ok(role_check("lab/analysis-code", &stub.url())));
    app.show(Screen::Forgejo);
    assert!(
        !app.can_open_pull_request(),
        "the legend offers P for a branch with nothing the real base lacks"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(
        app.status.contains("no commits") && app.status.contains("lacks"),
        "{}",
        app.status
    );
    // The count was measured against `origin/main`, so the refusal names that ref.
    assert!(
        app.status.contains("`origin/main`"),
        "the refusal does not name the base it measured: {}",
        app.status
    );
    assert!(
        !app.status.contains("press y or enter"),
        "the empty branch reached a preview: {}",
        app.status
    );
    assert_eq!(stub.requests().len(), 0, "a refusal made a request");
    let screen = render_at(72, 18, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend offers P at the floor: {screen}"
    );
    assert!(!screen.contains("open the pull request?"), "{screen}");
}

#[test]
fn p_refuses_after_the_remote_tracking_ref_is_pruned_even_with_a_witness() {
    let stub = Stub::start(vec![StubReply::json(
        r#"{"number":6,"title":"lab: witness","state":"open","head":{"ref":"feature"},"base":{"ref":"main"}}"#,
    )]);
    let root = scratch("p-witness-pruned");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    let run_id = "run-1700000000010-0010-1";
    let run_dir = write_run(&root, "scribe", run_id, "the push was real");
    write_plan(&root, &run_dir, run_id);
    gewu_multiverse::run::write_push_witness(&run_dir, "feature", "refs/remotes/origin/feature")
        .expect("the push witness is written");
    // The branch's remote-tracking ref is pruned after the run. The run's witness is
    // about the run, not this clone's remote; without the ref the gate must refuse.
    git(&clone, &["pack-refs", "--all"]);
    git(&clone, &["update-ref", "-d", "refs/remotes/origin/feature"]);
    assert!(
        !gewu_multiverse::workspace::branch_on_remote(&clone, "feature"),
        "the fixture's remote-tracking ref was not pruned"
    );
    app.reload();
    app.role_check = Some(Ok(role_check("lab/analysis-code", &stub.url())));
    app.show(Screen::Forgejo);
    assert!(
        !app.can_open_pull_request(),
        "the witness passed without the clone's remote-tracking ref"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        app.status.contains("not on the remote") && app.status.contains("push it first"),
        "the missing ref did not reach the refusal: {}",
        app.status
    );
    assert!(
        !app.status.contains("press y or enter"),
        "the missing ref reached a preview: {}",
        app.status
    );
    let screen = render_at(80, 24, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend offers P without the remote-tracking ref: {screen}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert_eq!(
        stub.requests().len(),
        0,
        "a refused gate wrote a pull request"
    );
}

#[test]
fn the_legend_adds_p_when_an_open_runs_own_record_ends_with_a_push() {
    let root = scratch("p-legend-run-end");
    let mut app = app_with(&root, ONE_AGENT);
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", true, false);
    record_identity(&app, "scribe");
    let run_id = "run-1700000000011-0011-1";
    let run_dir = write_running_run(&root, "scribe", run_id);
    write_plan(&root, &run_dir, run_id);
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    let before = render_at(80, 24, &app);
    assert!(
        !before.contains("P pull request"),
        "the running run was read as a push: {before}"
    );
    // The run pushes: git updates the clone's remote-tracking ref and the product
    // writes the witness beside the record. The screen stays open.
    git(
        &clone,
        &["update-ref", "refs/remotes/origin/feature", "feature"],
    );
    gewu_multiverse::run::write_push_witness(&run_dir, "feature", "refs/remotes/origin/feature")
        .expect("the push witness is written");
    finish_run(&run_dir, run_id);
    app.refresh_recent();
    let after = render_at(80, 24, &app);
    assert!(
        after.contains("P pull request"),
        "the legend kept the stale answer after the push:\n{after}"
    );
    assert!(app.can_open_pull_request(), "{}", app.status);
}

#[test]
fn the_legend_drops_p_when_the_branchs_remote_tracking_ref_is_pruned() {
    let root = scratch("p-legend-pruned-ref");
    let mut app = app_with(&root, ONE_AGENT);
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    let run_id = "run-1700000000012-0012-1";
    let run_dir = write_run(&root, "scribe", run_id, "no push witness here");
    write_plan(&root, &run_dir, run_id);
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    let before = render_at(80, 24, &app);
    assert!(
        before.contains("P pull request"),
        "the pushed branch has no P: {before}"
    );
    // Pack the refs, then prune the branch's remote-tracking ref behind the UI's
    // back; the gate now refuses and the legend must stop offering P.
    git(&clone, &["pack-refs", "--all"]);
    git(&clone, &["update-ref", "-d", "refs/remotes/origin/feature"]);
    let after = render_at(80, 24, &app);
    assert!(
        !after.contains("P pull request"),
        "the legend kept a stale P after the remote ref was pruned:\n{after}"
    );
    assert!(!app.can_open_pull_request(), "{}", app.status);
}

#[test]
fn the_legend_drops_p_when_the_local_branch_is_reset_while_shown() {
    let root = scratch("p-legend-local-reset");
    let mut app = app_with(&root, ONE_AGENT);
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    let before = render_at(80, 24, &app);
    assert!(
        before.contains("P pull request"),
        "the pushed branch has no P: {before}"
    );
    // A reset moves the local branch back to the measured base while every
    // remote-tracking ref and `packed-refs` stay byte-identical. The gate now refuses,
    // so the legend must stop naming P on the next frame.
    git(&clone, &["reset", "-q", "--hard", "origin/main"]);
    let after = render_at(80, 24, &app);
    assert!(
        !after.contains("P pull request"),
        "the legend kept a stale P after the local branch was reset:\n{after}"
    );
    assert!(!app.can_open_pull_request(), "{}", app.status);
}

#[test]
fn the_legend_gains_p_after_a_product_run_commits_without_pushing() {
    let root = scratch("p-legend-product-commit");
    // The branch is level with its base but already on the remote, so the gate starts
    // false for the one reason a local commit can fix: no commits the base lacks.
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code", false, true);
    let runner = runner(
        "run-commit",
        &format!(
            "#!/bin/sh\ncd '{}' || exit 1\ngit commit -q --allow-empty -m 'run-only commit, not pushed'\n",
            clone.display()
        ),
    );
    let mut app = app_with_runner(&root, ONE_AGENT, &runner);
    fs::write(
        &app.credentials_path,
        "[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the model key is written");
    record_identity(&app, "scribe");
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "do one thing\n").expect("the instruction is written");
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    let before = render_at(80, 24, &app);
    assert!(
        !before.contains("P pull request"),
        "a branch level with its base was offered as openable:\n{before}"
    );
    // The product's own `s` run commits in the clone and does not push. The Forgejo
    // screen stays open, so the legend must gain P as soon as the move is visible.
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(
        app.status.contains("started `scribe`"),
        "the product run did not start: {}",
        app.status
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while app.supervisor.running_count() > 0 && Instant::now() < deadline {
        app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        app.supervisor.running_count(),
        0,
        "the product run did not end: {}",
        app.status
    );
    let after = render_at(80, 24, &app);
    assert!(
        after.contains("P pull request"),
        "the legend kept the stale answer after the run committed:\n{after}"
    );
    assert!(app.can_open_pull_request(), "{}", app.status);
}

#[test]
fn a_sibling_clones_push_does_not_stand_for_this_clone() {
    let stub = Stub::start(vec![StubReply::json(
        r#"{"number":7,"title":"lab: sibling","state":"open","head":{"ref":"feature"},"base":{"ref":"main"}}"#,
    )]);
    let root = scratch("p-sibling");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    let layout = Layout::rooted_at(root.clone());
    let workspace = layout
        .agent("scribe")
        .expect("the agent path")
        .workspace()
        .to_path_buf();
    fs::create_dir_all(&workspace).expect("the agent workspace");
    let remote = bare_remote(&root);
    // A and B are two clones under one agent workspace. B is cloned before A's
    // feature is pushed, so B has no `origin/feature`; its own `feature` gains a
    // commit and is never pushed.
    let a = workspace.join("analysis-code");
    let b = workspace.join("other-tool");
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            a.to_str().expect("utf-8"),
        ],
    );
    git(&a, &["config", "user.email", "agent@example.invalid"]);
    git(&a, &["config", "user.name", "Agent"]);
    fs::write(a.join("README.md"), "hello\n").expect("a file");
    git(&a, &["add", "README.md"]);
    git(&a, &["commit", "-qm", "first"]);
    git(&a, &["push", "-q", "-u", "origin", "HEAD"]);
    git(
        &root,
        &[
            "clone",
            "-q",
            remote.to_str().expect("utf-8"),
            b.to_str().expect("utf-8"),
        ],
    );
    git(&b, &["config", "user.email", "agent@example.invalid"]);
    git(&b, &["config", "user.name", "Agent"]);
    git(&b, &["checkout", "-q", "-b", "feature"]);
    git(&b, &["commit", "-q", "--allow-empty", "-m", "sibling work"]);
    git(&a, &["checkout", "-q", "-b", "feature"]);
    git(&a, &["commit", "-q", "--allow-empty", "-m", "work"]);
    git(&a, &["push", "-q", "-u", "origin", "feature"]);
    assert!(
        !gewu_multiverse::workspace::branch_on_remote(&b, "feature"),
        "the sibling clone's feature is unexpectedly on its remote"
    );
    // A's run pushed its own clone; its plan names the shared workspace root, as the
    // product's own start does. The old per-workspace witness rule let it stand for B.
    record_identity(&app, "scribe");
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000020-0020-1",
        "the branch in the other clone is on the remote",
    );
    write_plan(&root, &run_dir, "run-1700000000020-0020-1");
    gewu_multiverse::run::write_push_witness(&run_dir, "feature", "refs/remotes/origin/feature")
        .expect("the push witness is written");
    app.reload();
    let mut check = role_check("lab/analysis-code", &stub.url());
    check.repositories.push(repo("lab/other-tool"));
    app.role_check = Some(Ok(check));
    app.repo_selected = 1;
    app.show(Screen::Forgejo);
    assert!(
        !app.can_open_pull_request(),
        "a sibling clone's push passed the gate: {}",
        app.status
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        app.status.contains("not on the remote") && app.status.contains("push it first"),
        "{}",
        app.status
    );
    assert!(
        !app.status.contains("press y or enter"),
        "the sibling's witness reached a preview: {}",
        app.status
    );
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert_eq!(
        stub.requests().len(),
        0,
        "the sibling clone's branch opened a pull request"
    );
}

#[test]
fn a_role_check_that_changes_the_default_branch_drops_the_cached_p() {
    let mut replies = role_check_replies("feature");
    replies.push(StubReply::json("[]"));
    let stub = Stub::start(replies);
    let root = scratch("p-role-change");
    let config = endpoint_config(&stub.url(), "http://127.0.0.1:9/v1/chat/completions");
    let mut app = app_with(&root, &config);
    clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        None,
        "forgejo_token",
        "synthetic-role-token",
    )
    .expect("the role token is written");
    // The gate passes while the server's default branch is `main`.
    app.role_check = Some(Ok(role_check("lab/analysis-code", &stub.url())));
    app.show(Screen::Forgejo);
    assert!(app.can_open_pull_request(), "{}", app.status);
    assert!(render_at(120, 40, &app).contains("P pull request"));
    // The server changes its default branch to the checked-out branch. `c` applies
    // the new view; the cached `P` must not outlive it.
    app.on_key(KeyEvent::from(KeyCode::Char('c')));
    app.wait_for_probe();
    assert_eq!(
        app.selected_repository()
            .and_then(|repository| repository.default_branch.as_deref()),
        Some("feature")
    );
    assert!(
        !app.can_open_pull_request(),
        "the cached legend answer outlived the role check: {}",
        app.status
    );
    let screen = render_at(120, 40, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend still offers P after the base changed:\n{screen}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        app.status.contains("base branch"),
        "the base branch did not reach the refusal: {}",
        app.status
    );
    assert_eq!(
        stub.requests().len(),
        3,
        "the role check and its lookup made the only requests; P wrote something"
    );
}

#[test]
fn removing_the_agents_token_drops_the_cached_p() {
    let root = scratch("p-token-gone");
    let mut app = app_with(&root, ONE_AGENT);
    clone_fixture(&root, "scribe", "lab/analysis-code", true, true);
    record_identity(&app, "scribe");
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.show(Screen::Forgejo);
    assert!(app.can_open_pull_request(), "{}", app.status);
    assert!(render_at(120, 40, &app).contains("P pull request"));
    // The token is removed while the screen is open. The login and email stay; the
    // action still cannot be made, so `P` must disappear.
    let removed = gewu_multiverse::credentials::remove_agent_secrets(
        &app.credentials_path,
        &["scribe".to_owned()],
    )
    .expect("the agent token is removed");
    assert!(
        removed.iter().any(|label| label.contains("Forgejo token")),
        "the fixture did not remove a Forgejo token: {removed:?}"
    );
    assert!(
        !app.can_open_pull_request(),
        "the cached legend answer outlived the token: {}",
        app.status
    );
    let screen = render_at(120, 40, &app);
    assert!(
        !screen.contains("P pull request"),
        "the legend still offers P without the agent's token:\n{screen}"
    );
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    assert!(
        app.status.contains("no Forgejo token"),
        "the missing token did not reach the refusal: {}",
        app.status
    );
}

// 2 — an overlay owns its footer and the frame stays whole.

#[test]
fn every_overlay_footer_names_only_its_own_keys() {
    let root = scratch("overlay-footer");
    type OverlayOpener = Box<dyn Fn(&mut App, &Path)>;
    let mut opens: Vec<(&str, &str, OverlayOpener)> = vec![
        (
            "add form",
            "ctrl-s writes it",
            Box::new(|app: &mut App, _root: &Path| app.request_new_agent()),
        ),
        (
            "endpoint editor",
            "tab/up/down move",
            Box::new(|app: &mut App, _root: &Path| {
                app.show(Screen::AgentConfig);
                app.on_key(KeyEvent::from(KeyCode::Char('E')));
            }),
        ),
        (
            "secret prompt",
            "ctrl-u clears it",
            Box::new(|app: &mut App, _root: &Path| {
                app.request_secret(SecretTarget::ModelKey);
            }),
        ),
        (
            "address prompt",
            "ctrl-u clears it",
            Box::new(|app: &mut App, _root: &Path| app.request_url_edit()),
        ),
        (
            "removal confirmation",
            "y or enter remove it",
            Box::new(|app: &mut App, _root: &Path| app.request_remove_agent()),
        ),
        (
            "leftover confirmation",
            "y or enter delete them",
            Box::new(|app: &mut App, _root: &Path| {
                gewu_multiverse::credentials::record_secret(
                    &app.credentials_path,
                    Some("model_keys"),
                    "gone-agent",
                    "synthetic-leftover",
                )
                .expect("the leftover secret is written");
                app.reload();
                app.request_forget_secrets();
            }),
        ),
        (
            "pull request preview",
            "y or enter opens it",
            Box::new(|app: &mut App, _root: &Path| {
                app.confirm_pull_request = Some(PullRequestPreview {
                    agent: "scribe".to_owned(),
                    login: "ken".to_owned(),
                    repository: "lab/analysis-code".to_owned(),
                    namespace: "lab".to_owned(),
                    name: "analysis-code".to_owned(),
                    number: None,
                    branch: "feature".to_owned(),
                    base: "main".to_owned(),
                    title: "the proposed title".to_owned(),
                    body: "the proposed body".to_owned(),
                    text_source: "pull-request.txt".to_owned(),
                });
            }),
        ),
        (
            "editor confirmation",
            "enter opens it",
            Box::new(|app: &mut App, root: &Path| {
                app.editor_confirm = Some(PendingAction {
                    kind: ActionKind::Edit,
                    path: root.join("instruction.md"),
                    label: "the instruction of `scribe`".to_owned(),
                    before: None,
                    target: None,
                    created: false,
                    template: None,
                });
            }),
        ),
        (
            "key help",
            "esc closes it",
            Box::new(|app: &mut App, _root: &Path| app.toggle_help()),
        ),
        (
            "read-only terms",
            "esc closes this overlay",
            Box::new(|app: &mut App, _root: &Path| app.review_acknowledgment()),
        ),
    ];
    for (label, expected, open) in &mut opens {
        let mut app = app_with(&root, ONE_AGENT);
        app.show_acknowledgment = false;
        open(&mut app, &root);
        assert!(app.overlay_owns_keys(), "{label} did not open");
        assert!(app.overlay_owns_screen(), "{label} did not own the screen");
        assert!(
            app.overlay_footer_keys().is_some(),
            "{label} has no overlay footer"
        );
        for (width, height) in [(72u16, 18u16), (80, 24), (120, 40)] {
            let screen = render_at(width, height, &app);
            let footer: String = screen.lines().rev().take(3).collect::<Vec<_>>().join("\n");
            assert!(
                footer.contains(*expected),
                "{label} at {width}x{height} does not name its own key `{expected}`:\n{footer}"
            );
            for forbidden in [
                "1-6 screen",
                "s start",
                "x stop",
                "p endpoint",
                "q quit",
                "tab or left/right screen",
                "up/down select",
            ] {
                assert!(
                    !footer.contains(forbidden),
                    "{label} at {width}x{height} advertises `{forbidden}` in its footer:\n{footer}"
                );
            }
        }
    }

    // The first-run acknowledgment is an overlay in the same set even on a fresh
    // configuration, and its footer owns only its answer keys.
    let fresh = scratch("overlay-first-run");
    fs::write(fresh.join("config.toml"), ONE_AGENT).expect("the configuration");
    let fresh_app = App::with_runner(
        Layout::rooted_at(fresh.clone()),
        runner("first-run-overlay", "#!/bin/sh\nexit 0\n"),
    );
    assert!(fresh_app.show_acknowledgment, "the first-run overlay is up");
    assert!(fresh_app.overlay_owns_keys());
    assert!(fresh_app.overlay_footer_keys().is_some());
    let first = render_at(80, 24, &fresh_app);
    let footer: String = first.lines().rev().take(3).collect::<Vec<_>>().join("\n");
    assert!(footer.contains("y records it once"), "{footer}");
    assert!(!footer.contains("1-6 screen"), "{footer}");
    assert!(!footer.contains("s start"), "{footer}");
}

#[test]
fn the_first_run_and_review_keep_the_title_tab_bar_and_answer() {
    let root = scratch("overlay-frame");
    fs::write(root.join("config.toml"), ONE_AGENT).expect("the configuration");
    let mut app = App::with_runner(
        Layout::rooted_at(root.clone()),
        runner("first-run-runner", "#!/bin/sh\nexit 0\n"),
    );
    assert!(app.show_acknowledgment, "the first-run overlay is up");
    for (width, height) in [(80u16, 24u16), (72, 18)] {
        let screen = render_at(width, height, &app);
        assert!(
            screen.contains("gewu-multiverse"),
            "title lost at {width}x{height}:\n{screen}"
        );
        assert!(
            screen.contains("1 Agents 2 Agent"),
            "tab bar lost at {width}x{height}:\n{screen}"
        );
        assert!(
            screen.contains("y — I understand, record it once"),
            "answer row lost at {width}x{height}:\n{screen}"
        );
        assert!(
            !screen.contains("1-6 screen") && !screen.contains("s start"),
            "first-run footer advertises screen keys at {width}x{height}:\n{screen}"
        );
    }
    app.ack_review = true;
    let review = render_at(80, 24, &app);
    assert!(review.contains("The terms you acknowledged — read-only"));
    assert!(review.contains("esc closes this; [ and ] scroll; the record is unchanged"));
}

#[test]
fn an_overlay_paints_one_full_width_band_and_cuts_no_panel_border() {
    let root = scratch("overlay-band");
    for open in ["add form", "endpoint editor", "removal", "help", "review"] {
        let mut app = app_with(&root, ONE_AGENT);
        match open {
            "add form" => app.request_new_agent(),
            "endpoint editor" => {
                app.show(Screen::AgentConfig);
                app.on_key(KeyEvent::from(KeyCode::Char('E')));
            }
            "removal" => app.request_remove_agent(),
            "help" => app.toggle_help(),
            "review" => app.review_acknowledgment(),
            _ => unreachable!(),
        }
        let screen = render_at(120, 40, &app);
        // The title frame and the overlay's own box are the only top-left corners;
        // a cut Agents table would add another.
        let corners = screen.matches('┌').count();
        assert_eq!(
            corners, 2,
            "{open} cut through the panel behind it at 120x40:\n{screen}"
        );
    }
}

// 3 — "no repositories" only when a check found none.

#[test]
fn a_reopened_forgejo_does_not_present_an_unknown_list_as_no_repositories() {
    let root = scratch("forgejo-unknown");
    let mut app = app_with(&root, ONE_AGENT);
    app.role_check = None;
    app.role_checked_at_ms = None;
    app.persisted_role_check_stamp = Some(RoleCheckStamp {
        checked_at_ms: gewu_multiverse::time::now_unix_ms().saturating_sub(180_000),
        machine: None,
        outcome: Some("member".to_owned()),
    });
    app.show(Screen::Forgejo);
    for (width, height) in [(72u16, 18u16), (80, 24)] {
        let screen = render_at(width, height, &app);
        assert!(
            !screen.contains("no repositories"),
            "an unknown list is presented as zero at {width}x{height}:\n{screen}"
        );
        assert!(
            screen.contains("not checked this session"),
            "the unknown state is not named at {width}x{height}:\n{screen}"
        );
    }

    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        "https://forgejo.example.org",
    )));
    app.role_check
        .as_mut()
        .expect("set")
        .as_mut()
        .expect("ok")
        .repositories
        .clear();
    let checked_empty = render_at(72, 18, &app);
    assert!(
        checked_empty.contains("no repositories"),
        "a check that found none does not say so:\n{checked_empty}"
    );
}

// 4 — one failure, one cause, the right field, the right screen.

#[test]
fn route_not_found_names_the_path_only_and_points_e_at_screen_two() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"no route"}"#),
        StubReply::status(404, r#"{"error":"no models"}"#),
    ]);
    let root = scratch("failure-route");
    let config = endpoint_config("https://forgejo.example.org", &stub.url());
    let mut app = app_with(&root, &config);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-key",
    )
    .expect("the key is recorded");
    app.show(Screen::AgentConfig);
    app.request_endpoint_test();
    app.wait_for_probe();
    assert!(app.status.contains("route_not_found"), "{}", app.status);
    assert!(
        normalised(&app.status).contains(&normalised("press 2 and E")),
        "the correction does not name the screen where E acts: {}",
        app.status
    );
    assert_eq!(
        app.status.matches("press 2").count(),
        1,
        "the route names screen 2 more than once: {}",
        app.status
    );
    assert_eq!(
        app.status.matches("full completions path").count(),
        1,
        "the route states its correction more than once: {}",
        app.status
    );
    assert_eq!(
        app.status.matches("model id").count(),
        0,
        "a models request that never answered was read as a model correction: {}",
        app.status
    );
    assert!(
        !app.status.contains("did not list the model"),
        "a models request that never answered was read as a model fact: {}",
        app.status
    );
    assert!(app.status.contains("/chat/completions"), "{}", app.status);
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains(&format!("endpoint {}", stub.url())) && floor.contains("model lab-chat-1"),
        "the visible status lost the endpoint/model facts at 72x18:\n{floor}"
    );
}

#[test]
fn model_not_found_points_at_the_model_field() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"no model"}"#),
        StubReply::json(r#"{"object":"list","data":[{"id":"another-model"}]}"#),
    ]);
    let root = scratch("failure-model");
    let config = endpoint_config("https://forgejo.example.org", &stub.url());
    let mut app = app_with(&root, &config);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-key",
    )
    .expect("the key is recorded");
    app.show(Screen::AgentConfig);
    app.request_endpoint_test();
    app.wait_for_probe();
    assert!(app.status.contains("model_not_found"), "{}", app.status);
    let flat = normalised(&app.status);
    assert!(
        flat.contains("model id"),
        "the model field is not named: {}",
        app.status
    );
    assert_eq!(
        app.status.matches("model id").count(),
        1,
        "the model correction is stated more than once: {}",
        app.status
    );
    assert_eq!(
        app.status.matches("press 2").count(),
        1,
        "the model route names screen 2 more than once: {}",
        app.status
    );
    assert!(
        !flat.contains("full completions path"),
        "the model failure still corrects the endpoint path: {}",
        app.status
    );
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains("endpoint ") && floor.contains("model lab-chat-1"),
        "the visible status lost the endpoint/model facts at 72x18:\n{floor}"
    );
}

#[test]
fn a_forgejo_page_path_is_told_to_use_the_instance_root() {
    let stub = Stub::start(vec![StubReply::status(404, r#"{"error":"not found"}"#)]);
    let root = scratch("forgejo-page-path");
    let config = endpoint_config(
        &format!("http://{}/explore/repos", stub.addr()),
        "http://127.0.0.1:9/v1/chat/completions",
    );
    let mut app = app_with(&root, &config);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        None,
        "forgejo_token",
        "synthetic-role-token",
    )
    .expect("the role token is recorded");
    app.request_role_check();
    app.wait_for_probe();
    let flat = normalised(&app.status);
    assert!(
        flat.contains("instance root"),
        "the page path is not corrected: {}",
        app.status
    );
    assert!(
        flat.contains("/explore/repos"),
        "the typed page path is not named: {}",
        app.status
    );
    assert!(
        flat.contains("instead") || flat.contains("use the instance root"),
        "{}",
        app.status
    );
    app.show(Screen::Forgejo);
    let screen = render_at(100, 30, &app);
    assert!(
        normalised(&screen).contains(&normalised("instance root")),
        "the panel does not carry the correction:\n{screen}"
    );
}

// 5 — the state and model columns are paid before a long name.

#[test]
fn a_long_name_is_shortened_before_the_model_id_is() {
    let root = scratch("width-order");
    let config = ONE_AGENT.replace(
        "name = \"scribe\"",
        "name = \"reverify-long-name-agent-width\"",
    );
    let config = config.replace("model = \"lab-chat-1\"", "model = \"lab-chat-1-long\"");
    let mut app = app_with(&root, &config);
    app.show(Screen::Agents);
    for (width, height) in [(80u16, 24u16), (72, 18)] {
        let screen = render_at(width, height, &app);
        assert!(
            screen.contains("lab-chat-1-long"),
            "the model was shortened before the name at {width}x{height}:\n{screen}"
        );
        assert!(
            !screen.contains("reverify-long-name-agent-width"),
            "the long name was shown whole while the model lost room at {width}x{height}:\n{screen}"
        );
        assert!(
            screen.contains('…'),
            "the shortened name has no marker:\n{screen}"
        );
    }
}

// 6 — a bordered panel drops whole lines before the pinned answer row.

#[test]
fn the_remove_confirmation_never_draws_half_a_wrapped_sentence() {
    let root = scratch("whole-lines");
    let config = ONE_AGENT.to_owned();
    let mut app = app_with(&root, &config);
    let paths = Layout::rooted_at(root.clone())
        .agent("scribe")
        .expect("the agent path");
    fs::create_dir_all(paths.config_dir()).expect("the agent directory");
    fs::create_dir_all(app.state_root().join("scribe")).expect("the run directory");
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-model-key",
    )
    .expect("the model key is written");
    let name = "scribe".to_owned();
    app.confirm_remove = Some(name.clone());
    let answer = "y or enter remove it";
    let head = "its model key in";
    let tail = "press D on the Credentials screen";
    let first = render_at(72, 18, &app);
    assert!(
        first.contains(answer),
        "the answer row is missing:\n{first}"
    );
    let first_flat = normalised(&first);
    assert_eq!(
        first_flat.contains(head),
        first_flat.contains(tail),
        "the credentials sentence is drawn only in part above the answer:\n{first}"
    );
    let mut seen = first_flat.contains(head) && first_flat.contains(tail);
    for _ in 0..40 {
        if seen {
            break;
        }
        app.on_key(KeyEvent::from(KeyCode::Char(']')));
        let page = render_at(72, 18, &app);
        assert!(page.contains(answer), "the answer left the floor:\n{page}");
        let flat = normalised(&page);
        if flat.contains(head) {
            assert!(
                flat.contains(tail),
                "a scrolled page still draws half the credentials sentence:\n{page}"
            );
            seen = true;
        }
    }
    assert!(seen, "the whole credentials sentence was never reachable");
}

// 7 — a quiet hub, with the record's facts behind R and the scheduler's behind ?.

#[test]
fn the_hub_draws_one_risk_line_and_one_scheduler_line() {
    let root = scratch("quiet-hub");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Agents);
    for (width, height) in [(80u16, 24u16), (72, 18)] {
        let screen = render_at(width, height, &app);
        assert!(screen.contains("risk recorded on"), "{screen}");
        assert!(screen.contains("R shows the record"), "{screen}");
        assert!(!screen.contains("the risk was acknowledged on"), "{screen}");
        assert!(
            normalised(&screen).contains(&normalised("no scheduler — runs start with s")),
            "the scheduler line is missing at {width}x{height}:\n{screen}"
        );
        assert!(
            !screen.contains("recurring run needs your own timer"),
            "{screen}"
        );
    }

    app.review_acknowledgment();
    let review = render_at(80, 24, &app);
    let flat = normalised(&review);
    assert!(flat.contains("the risk was acknowledged on"), "{review}");
    assert!(flat.contains("this build is"), "{review}");
    assert!(flat.contains("plain file"), "{review}");
    assert!(flat.contains("made on this machine"), "{review}");

    app.show_acknowledgment = false;
    app.show_help = true;
    let help = render_at(120, 40, &app);
    assert!(
        normalised(&help).contains(&normalised("Scheduler: none")),
        "the help overlay does not hold the scheduler facts:\n{help}"
    );
    assert!(
        normalised(&help).contains(&normalised("recurring run needs your own timer")),
        "{help}"
    );
}

// The small copies: a marker legend, and s/x drawn once — by the action row where it
// names them, by the global footer everywhere else.
#[test]
fn the_runs_header_legends_the_marker_and_s_x_are_drawn_once_per_screen() {
    let root = scratch("small-copy");
    let mut app = app_with(&root, ONE_AGENT);
    write_run(&root, "gone", "run-1700000000004-0004-1", "reply");
    app.reload();
    app.show(Screen::Runs);
    let screen = render_at(80, 24, &app);
    assert!(
        screen.contains("* earlier"),
        "the marker has no legend:\n{screen}"
    );

    // The page map's rule: the global footer omits `s`/`x` only where the screen's own
    // action row names them, so each screen draws each key exactly once.
    for expected in Screen::ALL {
        app.show(expected);
        let rendered = render_at(120, 40, &app);
        assert_eq!(
            rendered.matches("s start   ").count(),
            1,
            "`s start` is drawn zero times or twice on {}:\n{rendered}",
            expected.title()
        );
        assert_eq!(
            rendered.matches("x stop   ").count(),
            1,
            "`x stop` is drawn zero times or twice on {}:\n{rendered}",
            expected.title()
        );
    }
}
