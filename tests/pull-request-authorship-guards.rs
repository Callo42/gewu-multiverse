//! Guards for the author-qualified pull-request routes, the floor's endpoint facts
//! and selected repository row, and the page map's endpoint table.
//!
//! Every guard drives the product's own `App`, key handling, Forgejo client and
//! renderer against the loopback test stub.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{App, EndpointFailure, EndpointTest, Screen};
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
concurrency = 1
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "gewu-pr-owner-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), runner("runner"));
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
    app.show_acknowledgment = false;
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

/// A clone on a pushed `feature` branch with one commit beyond `main`.
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
    git(
        &destination,
        &["commit", "-q", "--allow-empty", "-m", "work"],
    );
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
    gewu_multiverse::run::write_push_witness(
        run_dir,
        branch,
        &format!("refs/remotes/origin/{branch}"),
    )
    .expect("the push witness is written");
}

fn forgejo_ready_app(root: &Path, stub: &Stub, clone: &Path) -> App {
    let mut app = app_with(root, &config_for(&format!("http://{}", stub.addr())));
    let _ = clone;
    record_identity(&app, "scribe");
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        &format!("http://{}", stub.addr()),
    )));
    app.show(Screen::Forgejo);
    app
}

fn open_pr(number: u64, title: &str, head: &str, base: &str, login: &str) -> String {
    format!(
        r#"{{"number":{number},"title":"{title}","state":"open","head":{{"ref":"{head}","repo":{{"full_name":"lab/analysis-code"}}}},"base":{{"ref":"{base}","repo":{{"full_name":"lab/analysis-code"}}}},"user":{{"login":"{login}"}}}}"#
    )
}

fn open_pr_without_author(number: u64, title: &str) -> String {
    format!(
        r#"{{"number":{number},"title":"{title}","state":"open","head":{{"ref":"feature","repo":{{"full_name":"lab/analysis-code"}}}},"base":{{"ref":"main","repo":{{"full_name":"lab/analysis-code"}}}}}}"#
    )
}

fn open_pr_without_head_repo(number: u64, title: &str, login: &str) -> String {
    format!(
        r#"{{"number":{number},"title":"{title}","state":"open","head":{{"ref":"feature"}},"base":{{"ref":"main","repo":{{"full_name":"lab/analysis-code"}}}},"user":{{"login":"{login}"}}}}"#
    )
}

fn created_pr(title: &str, login: &str) -> String {
    format!(
        r#"{{"number":1,"title":"{title}","state":"open","head":{{"ref":"feature","repo":{{"full_name":"lab/analysis-code"}}}},"base":{{"ref":"main","repo":{{"full_name":"lab/analysis-code"}}}},"user":{{"login":"{login}"}}}}"#
    )
}

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

// The author decides the route, at every width, and no write happens.

#[test]
fn p_names_another_accounts_request_and_offers_no_update_route() {
    let list = format!(
        "[{}]",
        open_pr(
            10,
            "A patch from another account",
            "feature",
            "main",
            "someone-else"
        )
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("p-other");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("#10")),
        "the refusal does not name the request: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("another account's request")),
        "the refusal does not say whose request it is: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("U updates only your own request")),
        "the refusal does not qualify U: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("press U")),
        "the refusal still offers U as the update route: {}",
        app.status
    );
    assert!(
        app.confirm_pull_request.is_none(),
        "a foreign request was previewed"
    );
    for (width, height) in [(72u16, 18u16), (80, 24), (120, 40)] {
        let screen = normalised(&render_at(width, height, &app));
        assert!(
            screen.contains(&normalised("#10")) && screen.contains(&normalised("another account")),
            "the {width}x{height} screen does not keep the author fact:\n{screen}"
        );
    }
    let requests = stub.requests();
    assert!(
        requests.iter().all(|request| request.method != "POST"),
        "P wrote a foreign request: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.method != "PATCH"),
        "P patched a foreign request: {requests:?}"
    );
}

#[test]
fn u_refuses_another_accounts_request_and_does_not_offer_p() {
    let list = format!(
        "[{}]",
        open_pr(
            10,
            "A patch from another account",
            "feature",
            "main",
            "someone-else"
        )
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("u-other");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("#10"))
            && status.contains(&normalised("another account's request")),
        "U does not name the foreign request: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("U updates only your own request")),
        "U does not qualify what it updates: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("press P")),
        "U promises P while the pair is occupied: {}",
        app.status
    );
    assert!(
        app.confirm_pull_request.is_none(),
        "U previewed a foreign request"
    );
    for (width, height) in [(72u16, 18u16), (80, 24), (120, 40)] {
        let screen = normalised(&render_at(width, height, &app));
        assert!(
            screen.contains(&normalised("#10")) && screen.contains(&normalised("another account")),
            "the {width}x{height} screen does not keep the author fact:\n{screen}"
        );
    }
    let requests = stub.requests();
    assert!(
        requests.iter().all(|request| request.method != "PATCH"),
        "U patched a foreign request: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.method != "POST"),
        "U opened a request: {requests:?}"
    );
}

#[test]
fn the_runs_note_does_not_offer_u_for_another_accounts_request() {
    let list = format!(
        "[{}]",
        open_pr(
            10,
            "A patch from another account",
            "feature",
            "main",
            "someone-else"
        )
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("runs-other");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);
    let run_dir = write_run(
        &root,
        "scribe",
        "run-1700000000002-0002-1",
        "A pushed reply",
    );
    write_plan(&root, &run_dir, "run-1700000000002-0002-1");
    publish_witness(&run_dir, "feature");
    app.reload();
    app.role_check = Some(Ok(role_check(
        "lab/analysis-code",
        &format!("http://{}", stub.addr()),
    )));
    app.show(Screen::Forgejo);
    // Learn the open request through the product's own lookup, then read the note.
    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.show(Screen::Runs);

    // The note is below the run list; put its own first row at the top of the
    // note window at each width, the row a person reaches with the panel's page keys.
    for (width, height) in [(72u16, 18u16), (80, 24), (120, 40)] {
        app.panel_scroll = 0;
        let _ = render_at(width, height, &app);
        let rows = gewu_multiverse::ui::panel_rows_text(&app);
        let next = rows
            .iter()
            .position(|row| row.contains("next step: the run pushed"))
            .expect("the Runs note has no next-step row");
        app.panel_scroll = next;
        let found = normalised(&render_at(width, height, &app));
        assert!(
            found.contains(&normalised("#10")) && found.contains(&normalised("another account")),
            "the {width}x{height} Runs note does not name the foreign request:\n{found}"
        );
        assert!(
            !found.contains(&normalised("press U")),
            "the {width}x{height} Runs note offers U for a foreign request:\n{found}"
        );
        assert!(
            found.contains(&normalised("U updates only your own request")),
            "the {width}x{height} Runs note does not qualify U:\n{found}"
        );
    }
    let requests = stub.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.method != "POST" && request.method != "PATCH"),
        "the Runs note path wrote a request: {requests:?}"
    );
}

#[test]
fn an_unnamed_author_is_not_the_agents_own() {
    let list = format!(
        "[{}]",
        open_pr_without_author(31, "A request with no author")
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("u-unnamed");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("#31")) && status.contains(&normalised("author is not named")),
        "U does not name the missing author: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("press P")),
        "U offers P while the pair is occupied: {}",
        app.status
    );
    assert!(
        app.confirm_pull_request.is_none(),
        "U previewed a request with no author"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.method != "PATCH"),
        "U patched a request with no author: {:?}",
        stub.requests()
    );
}

#[test]
fn a_head_without_a_repository_is_not_a_match() {
    let list = format!(
        "[{}]",
        open_pr_without_head_repo(32, "A request with no head repository", "ken")
    );
    let stub = Stub::start(vec![StubReply::json(list)]);
    let root = scratch("u-no-head-repo");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("no open pull request to update")),
        "the headless-repository request was treated as a match: {}",
        app.status
    );
    assert!(
        app.confirm_pull_request.is_none(),
        "U previewed a request whose head names no repository"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|request| request.method != "PATCH"),
        "U patched a request whose head names no repository: {:?}",
        stub.requests()
    );
}

#[test]
fn a_create_conflict_does_not_promise_that_u_can_update_it() {
    let stub = Stub::start(vec![
        StubReply::json("[]"),
        StubReply::status(
            409,
            r#"{"message":"pull request already exists for feature","url":"http://example.invalid"}"#,
        ),
    ]);
    let root = scratch("p-409-authorship");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let status = normalised(&app.status);
    assert!(
        status.contains(&normalised("conflict"))
            && status.contains(&normalised("does not name its author")),
        "the conflict does not state the missing author: {}",
        app.status
    );
    assert!(
        status.contains(&normalised("U updates only your own request")),
        "the conflict does not qualify U: {}",
        app.status
    );
    assert!(
        !status.contains(&normalised("press U on the Forgejo screen (4) to update")),
        "the conflict still promises an update through U: {}",
        app.status
    );
    assert!(
        !app.status.contains('{') && !app.status.contains("http://example.invalid"),
        "the raw conflict body is on screen: {}",
        app.status
    );
    let requests = stub.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1,
        "the create was not attempted once: {requests:?}"
    );
    assert!(
        requests.iter().all(|request| request.method != "PATCH"),
        "the conflict path patched a request: {requests:?}"
    );
}

// The U legend and help name the branch's own request.

#[test]
fn the_u_legend_and_help_name_this_branchs_request() {
    let root = scratch("u-copy");
    let mut app = app_with(&root, ONE_AGENT);
    app.role_check = Some(Ok(role_check("lab/analysis-code", "http://127.0.0.1:9")));
    app.show(Screen::Forgejo);
    for (width, height) in [(72u16, 18u16), (80, 24), (120, 40)] {
        app.panel_scroll = 0;
        let mut found = String::new();
        for _ in 0..10 {
            let screen = normalised(&render_at(width, height, &app));
            if screen.contains(&normalised("U update this branch's request")) {
                found = screen;
                break;
            }
            app.on_key(KeyEvent::from(KeyCode::Char(']')));
        }
        assert!(
            !found.is_empty(),
            "the {width}x{height} legend does not describe U after paging"
        );
        assert!(
            !found.contains(&normalised("newest")),
            "the {width}x{height} legend still says newest:\n{found}"
        );
    }
    app.on_key(KeyEvent::from(KeyCode::Char('?')));
    let help = normalised(&render_at(120, 40, &app));
    assert!(
        help.contains(&normalised("U update this branch's request")),
        "the help still describes another request:\n{help}"
    );
    assert!(
        !help.contains(&normalised("U update the newest")),
        "the help still describes another request:\n{help}"
    );
}

// The endpoint facts lead at the floor, and the selected repository is never dropped.

#[test]
fn the_endpoint_status_keeps_its_facts_at_the_floor() {
    let long =
        "the model is overloaded and the upstream request should be retried later; ".repeat(12);
    let stub = Stub::start(vec![StubReply::status(
        500,
        format!(
            r#"{{"error":{{"message":"fixture upstream failed: {long}","type":"server_error"}}}}"#
        ),
    )]);
    let root = scratch("endpoint-status-floor");
    let config = config_for("http://127.0.0.1:9")
        .replace("http://127.0.0.1:9/v1/chat/completions", &stub.url());
    let mut app = app_with(&root, &config);
    gewu_multiverse::credentials::record_secret(
        &app.credentials_path,
        Some("model_keys"),
        "scribe",
        "synthetic-model-key",
    )
    .expect("the model key is written");
    app.reload();
    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char('p')));
    app.wait_for_probe();
    let expected = format!("endpoint {} — model lab-chat-1", stub.url());
    let screen = normalised(&render_at(72, 18, &app));
    assert!(
        screen.contains(&normalised(&expected)),
        "the endpoint and model facts left the floor status:\n{screen}"
    );
    assert!(
        screen.contains(&normalised("press p")),
        "the endpoint failure lost its test key at the floor:\n{screen}"
    );
    assert!(
        !screen.contains(&normalised("endpoint test for")),
        "the status kept redundant prose before the facts:\n{screen}"
    );
}

#[test]
fn the_endpoint_test_describes_the_facts_before_the_recovery() {
    let root = scratch("endpoint-describe");
    let mut app = app_with(&root, ONE_AGENT);
    for (class, message, correction) in [
        (
            "server_error",
            "the provider said the upstream is overloaded and should be retried later",
            "press p",
        ),
        (
            "timeout",
            "the request timed out after 15 s",
            "check the endpoint address",
        ),
    ] {
        endpoint_failure(&mut app, class, message);
        let line = app
            .endpoint_test
            .as_ref()
            .expect("the test result")
            .describe();
        assert!(
            line.starts_with(
                "endpoint http://127.0.0.1:9/v1/chat/completions — model lab-chat-1 — "
            ),
            "{class} does not lead with its facts: {line}"
        );
        assert!(
            line.contains(correction),
            "{class} lost its one correction: {line}"
        );
    }
}

#[test]
fn the_selected_repository_is_drawn_after_an_open_at_the_floor() {
    let stub = Stub::start(vec![
        StubReply::json("[]"),
        StubReply::json(created_pr("A pushed reply", "ken")),
    ]);
    let root = scratch("floor-open");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('P')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(
        normalised(&app.status).contains(&normalised("opened #1")),
        "the open receipt is missing: {}",
        app.status
    );
    for after_page in [false, true] {
        if after_page {
            app.on_key(KeyEvent::from(KeyCode::Char(']')));
        }
        let floor = render_at(72, 18, &app);
        assert!(
            floor.contains("> lab/analysis-code"),
            "the selected repository left the floor after an open{}:\n{floor}",
            if after_page { " and paging" } else { "" }
        );
    }
}

#[test]
fn the_selected_repository_is_drawn_after_an_update_at_the_floor() {
    let list = format!(
        "[{}]",
        open_pr(1, "Proofread the guide", "feature", "main", "ken")
    );
    let patch = StubReply::json(created_pr("Proofread the guide", "ken"));
    let stub = Stub::start(vec![StubReply::json(list), patch]);
    let root = scratch("floor-update");
    let clone = clone_fixture(&root, "scribe", "lab/analysis-code");
    let mut app = forgejo_ready_app(&root, &stub, &clone);

    app.on_key(KeyEvent::from(KeyCode::Char('U')));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(
        normalised(&app.status).contains(&normalised("updated #1")),
        "the update receipt is missing: {}",
        app.status
    );
    for after_page in [false, true] {
        if after_page {
            app.on_key(KeyEvent::from(KeyCode::Char(']')));
        }
        let floor = render_at(72, 18, &app);
        assert!(
            floor.contains("> lab/analysis-code"),
            "the selected repository left the floor after an update{}:\n{floor}",
            if after_page { " and paging" } else { "" }
        );
    }
}

// The page map matches the endpoint recovery sentences that ship.

#[test]
fn the_page_map_endpoint_rows_match_the_single_corrections() {
    let map = include_str!("../docs/page-map.md");
    let row = |class: &str| {
        map.lines()
            .find(|line| line.starts_with(&format!("| `{class}`")))
            .unwrap_or_else(|| panic!("the page map has no {class} row"))
    };
    for class in ["bad_url", "missing_key", "bad_key"] {
        assert!(
            !row(class).contains("then `p`"),
            "the {class} row adds a test step its sentence does not: {}",
            row(class)
        );
    }
    let flat = map.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("If its author is the agent's own recorded login")
            && flat.contains("A head that names no repository is not a match"),
        "the page map does not state the author-qualified rule"
    );
}
