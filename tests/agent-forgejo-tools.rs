//! The Forgejo tools, driven through the product's own run loop.
//!
//! Every test here uses loopback stubs only: one model stub that answers with the
//! tool calls a model would make, and one Forgejo stub that records the request it
//! received. The real binary runs the real `agent_run` loop, writes the real
//! transcript and log.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::paths::{AgentPaths, Layout};
use gewu_multiverse::run::RunPlan;
use gewu_multiverse::test_stub::{Route, Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};

const BINARY: &str = env!("CARGO_BIN_EXE_gewu-multiverse");
const MODEL_KEY: &str = "gw-fixture-model-key";
const AGENT_TOKEN: &str = "gw-fixture-agent-token";
const ROLE_TOKEN: &str = "gw-fixture-role-token";
const ADMIN_TOKEN: &str = "gw-fixture-admin-token";
const REPO: &str = "lab/analysis-code";
const DIRECTORY: &str = "analysis-code";

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("gewu-tools-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
fn clone_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let remote = root.join("remote.git");
    git(root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    let layout = Layout::rooted_at(root.to_path_buf()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let workspace = paths.workspace().to_path_buf();
    fs::create_dir_all(&workspace).expect("the agent workspace");
    let destination = workspace.join(DIRECTORY);
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
    (workspace, destination)
}

fn write_credentials(path: &Path) {
    let text = format!(
        r#"model_key = "{MODEL_KEY}"
forgejo_token = "{ROLE_TOKEN}"
forgejo_admin_token = "{ADMIN_TOKEN}"

[forgejo_agent_tokens]
scribe = "{AGENT_TOKEN}"

[forgejo_logins]
scribe = "gewu-scribe"

[forgejo_emails]
scribe = "scribe@example.invalid"

forgejo_identity_mode = "managed"

[forgejo_identity_rights]
scribe = "issue-tools-v1"
"#
    );
    gewu_multiverse::mode::write_private(path, text.as_bytes()).expect("the credentials file");
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> StubReply {
    StubReply::json(
        serde_json::json!({
            "model": "lab-chat-1",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 4, "total_tokens": 8}
        })
        .to_string(),
    )
}

fn final_reply(text: &str) -> StubReply {
    StubReply::json(
        serde_json::json!({
            "model": "lab-chat-1",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": text}
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 4, "total_tokens": 8}
        })
        .to_string(),
    )
}

fn json_reply(body: serde_json::Value) -> StubReply {
    StubReply::json(body.to_string())
}

fn repo_json() -> StubReply {
    json_reply(serde_json::json!({
        "full_name": REPO,
        "name": "analysis-code",
        "default_branch": "main",
        "html_url": "http://forgejo.test/lab/analysis-code"
    }))
}

fn issue_json(number: u64, author: &str, is_pull: bool) -> StubReply {
    let mut body = serde_json::json!({
        "number": number,
        "title": "An assigned issue",
        "body": "the body of the issue",
        "state": "open",
        "html_url": format!("http://forgejo.test/{REPO}/issues/{number}"),
        "labels": [{"name": "bug"}, {"name": "docs"}],
        "updated_at": "2026-01-02T03:04:05Z",
        "user": {"login": author},
        "comments": 2,
        "repository": {"full_name": REPO}
    });
    if is_pull {
        body["pull_request"] = serde_json::json!({
            "html_url": format!("http://forgejo.test/{REPO}/pulls/{number}")
        });
    }
    json_reply(body)
}

fn comments_json() -> StubReply {
    json_reply(serde_json::json!([
        {
            "id": 21,
            "body": "first comment",
            "html_url": format!("http://forgejo.test/{REPO}/issues/12#issuecomment-21"),
            "user": {"login": "ken"},
            "created_at": "2026-01-02T04:05:06Z"
        }
    ]))
}

fn pulls_json(pulls: serde_json::Value) -> StubReply {
    json_reply(pulls)
}

fn pull_json(number: u64, author: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "A pull request",
        "state": "open",
        "html_url": format!("http://forgejo.test/{REPO}/pulls/{number}"),
        "head": {"ref": "feature", "repo": {"full_name": REPO}},
        "base": {"ref": "main", "repo": {"full_name": REPO}},
        "user": {"login": author}
    })
}

fn standard_routes(open_pulls: serde_json::Value) -> Vec<Route> {
    vec![
        Route::new(
            "GET",
            "/api/v1/repos/issues/search",
            json_reply(serde_json::json!([{
                "number": 12,
                "title": "An assigned issue",
                "body": "the body of the issue",
                "state": "open",
                "html_url": format!("http://forgejo.test/{REPO}/issues/12"),
                "labels": [{"name": "bug"}, {"name": "docs"}],
                "updated_at": "2026-01-02T03:04:05Z",
                "user": {"login": "ken"},
                "comments": 2,
                "repository": {"full_name": REPO}
            }])),
        ),
        Route::new(
            "GET",
            format!("/api/v1/repos/{REPO}/issues/12/comments"),
            comments_json(),
        ),
        Route::new(
            "GET",
            format!("/api/v1/repos/{REPO}/issues/12"),
            issue_json(12, "ken", false),
        ),
        Route::new(
            "POST",
            format!("/api/v1/repos/{REPO}/issues/12/comments"),
            StubReply::status(
                201,
                serde_json::json!({
                    "id": 31,
                    "body": "a comment",
                    "html_url": format!("http://forgejo.test/{REPO}/issues/12#issuecomment-31"),
                    "user": {"login": "gewu-scribe"},
                    "created_at": "2026-01-02T05:06:07Z"
                })
                .to_string(),
            ),
        ),
        Route::new(
            "GET",
            format!("/api/v1/repos/{REPO}/pulls?state=open&limit=50"),
            pulls_json(open_pulls.clone()),
        ),
        Route::new("GET", format!("/api/v1/repos/{REPO}"), repo_json()),
        Route::new(
            "POST",
            format!("/api/v1/repos/{REPO}/pulls"),
            StubReply::status(201, pull_json(3, "gewu-scribe").to_string()),
        ),
        Route::new(
            "PATCH",
            format!("/api/v1/repos/{REPO}/pulls/3"),
            StubReply::status(
                201,
                serde_json::json!({
                    "number": 3,
                    "title": "The updated title",
                    "state": "open",
                    "html_url": format!("http://forgejo.test/{REPO}/pulls/3"),
                    "head": {"ref": "feature", "repo": {"full_name": REPO}},
                    "base": {"ref": "main", "repo": {"full_name": REPO}},
                    "user": {"login": "gewu-scribe"}
                })
                .to_string(),
            ),
        ),
    ]
}

fn write_plan(
    root: &Path,
    paths: &AgentPaths,
    model: &Stub,
    run_id: &str,
    max_turns: u32,
) -> PathBuf {
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory");
    fs::create_dir_all(paths.skills_dir()).expect("the skills directory");
    fs::write(paths.instruction(), "Work the assigned issue.\n").expect("the instruction");
    RunPlan {
        run_id: run_id.to_owned(),
        agent: "scribe".to_owned(),
        workspace: paths.workspace().to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        endpoint: model.url(),
        model: "lab-chat-1".to_owned(),
        credentials: root.join("credentials.toml"),
        max_turns,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "none".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan is written");
    run_dir
}

fn write_context(run_dir: &Path, forgejo: &Stub) {
    let context = serde_json::json!({
        "url": format!("http://{}", forgejo.addr()),
        "login": "gewu-scribe",
        "repositories": [{"full_name": REPO, "directory": DIRECTORY}]
    });
    fs::write(run_dir.join("forgejo.json"), context.to_string()).expect("the context");
}

/// Run the real binary for one prepared run, and return the run directory.
fn run_binary(run_dir: &Path) -> std::process::ExitStatus {
    Command::new(BINARY)
        .arg("--run-agent")
        .arg(run_dir)
        .status()
        .expect("the agent process runs")
}

fn events(run_dir: &Path) -> Vec<String> {
    let file = transcript::read(run_dir).expect("the transcript reads back");
    file.records
        .iter()
        .map(|record| record.event().to_owned())
        .collect()
}

fn transcript_text(run_dir: &Path) -> String {
    fs::read_to_string(run_dir.join("transcript.jsonl")).expect("the transcript is readable")
}

fn log_text(run_dir: &Path) -> String {
    fs::read_to_string(run_dir.join("run.log")).expect("the log is readable")
}

fn assert_no_secret_in_tree(root: &Path, run_dir: &Path, model: &Stub) {
    for (label, text) in [
        ("transcript", transcript_text(run_dir)),
        ("log", log_text(run_dir)),
        (
            "plan",
            fs::read_to_string(run_dir.join("plan.json")).expect("the plan"),
        ),
        (
            "context",
            fs::read_to_string(run_dir.join("forgejo.json")).expect("the context"),
        ),
    ] {
        assert!(
            !text.contains(AGENT_TOKEN),
            "the {label} holds the agent token"
        );
        assert!(
            !text.contains(ROLE_TOKEN),
            "the {label} holds the role token"
        );
        assert!(
            !text.contains(ADMIN_TOKEN),
            "the {label} holds the admin token"
        );
    }
    // Every file the run and the workspace hold, including a `.git` the agent ran
    // git in. The credentials file itself is outside both and is not scanned.
    for dir in [run_dir.to_path_buf(), root.join("agents/scribe/workspace")] {
        let mut stack = vec![dir];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = fs::read(&path) {
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(
                        !text.contains(AGENT_TOKEN),
                        "{} holds the agent token",
                        path.display()
                    );
                }
            }
        }
    }
    for request in model.requests() {
        assert!(
            !request.body.contains(AGENT_TOKEN),
            "a model request holds the agent token"
        );
    }
}

#[test]
fn each_forgejo_tool_runs_through_the_real_loop_and_records_its_actions() {
    let root = scratch("all-tools");
    write_credentials(&root.join("credentials.toml"));
    let (workspace, _destination) = clone_fixture(&root);
    let _ = workspace;
    let model = Stub::start(vec![
        tool_reply("c1", "forgejo_list_assigned_issues", "{}"),
        tool_reply(
            "c2",
            "forgejo_read_issue_or_pull",
            &serde_json::json!({"repository": REPO, "number": 12}).to_string(),
        ),
        tool_reply(
            "c3",
            "forgejo_comment",
            &serde_json::json!({"repository": REPO, "number": 12, "body": "a comment"}).to_string(),
        ),
        tool_reply("c4", "forgejo_list_my_pull_requests", "{}"),
        tool_reply(
            "c5",
            "forgejo_open_pull_request",
            &serde_json::json!({"repository": REPO, "title": "Agent work", "body": "the body"})
                .to_string(),
        ),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-all-1", 12);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());

    let event_list = events(&run_dir);
    for wanted in ["forgejo_call", "forgejo_result"] {
        assert!(
            event_list.iter().any(|event| event == wanted),
            "the transcript has no {wanted}: {event_list:?}"
        );
    }
    let file = transcript::read(&run_dir).expect("the transcript");
    let results: Vec<&Record> = file
        .records
        .iter()
        .filter(|record| matches!(record, Record::ForgejoResult { .. }))
        .collect();
    assert_eq!(results.len(), 5, "{results:?}");
    // A list call names the cloned repositories it read, not an empty field.
    assert!(
        file.records.iter().any(|record| matches!(
            record,
            Record::ForgejoCall { tool, repository: Some(repository), .. }
                if tool == "forgejo_list_assigned_issues" && repository.contains(REPO)
        )),
        "the list call does not name its repository scope"
    );
    assert!(
        results.iter().any(|record| matches!(
            record,
            Record::ForgejoResult { action: Some(action), url: Some(url), .. }
                if action == "commented" && url.contains("issuecomment-31")
        )),
        "the comment action is missing: {results:?}"
    );
    assert!(
        results.iter().any(|record| matches!(
            record,
            Record::ForgejoResult { action: Some(action), number: Some(3), url: Some(url), .. }
                if action == "opened" && url.contains("/pulls/3")
        )),
        "the open action is missing: {results:?}"
    );
    let actions = gewu_multiverse::run::forgejo_actions(&run_dir);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(actions[0].sentence.contains("commented on"));
    assert!(actions[1].sentence.contains("opened #3"));

    // The Forgejo stub saw the agent's token on every call, and never the role or
    // administrator token. The log names the request and the token it carried without
    // printing a token value, so it can be evidence.
    let requests = forgejo.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.header("authorization").as_deref(),
            Some(format!("token {AGENT_TOKEN}").as_str()),
            "{} {} used another token",
            request.method,
            request.path
        );
        println!(
            "stub request: {} {} (authorization: the agent's own token)",
            request.method, request.path
        );
    }
    for (index, request) in model.requests().iter().enumerate() {
        println!(
            "model request {}: POST {} ({} bytes, no agent token)",
            index + 1,
            request.path,
            request.body.len()
        );
    }
    // The comment carried the model's body and the open carried its title.
    let comment = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.contains("/comments"))
        .expect("the comment request");
    assert!(comment.body.contains("a comment"), "{}", comment.body);
    let opened = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/pulls"))
        .expect("the open request");
    assert!(opened.body.contains("Agent work"), "{}", opened.body);

    assert_no_secret_in_tree(&root, &run_dir, &model);
    assert!(
        log_text(&run_dir).contains("forgejo tools: on as `gewu-scribe`"),
        "{}",
        log_text(&run_dir)
    );
}

#[test]
fn update_changes_only_the_agents_own_open_request() {
    let root = scratch("update");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "forgejo_update_pull_request",
            &serde_json::json!({
                "repository": REPO,
                "number": 3,
                "title": "The updated title",
                "body": "the updated body"
            })
            .to_string(),
        ),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([pull_json(
        3,
        "gewu-scribe"
    )])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-update-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let patch = forgejo
        .requests()
        .into_iter()
        .find(|request| request.method == "PATCH")
        .expect("the update was sent");
    assert!(patch.path.ends_with("/pulls/3"), "{}", patch.path);
    assert!(patch.body.contains("The updated title"), "{}", patch.body);
    assert!(patch.body.contains("the updated body"), "{}", patch.body);
    let actions = gewu_multiverse::run::forgejo_actions(&run_dir);
    assert!(
        actions.iter().any(|action| action.sentence.contains("updated #3")
            && action.url.contains("/pulls/3")),
        "{actions:?}"
    );
}

#[test]
fn another_accounts_open_request_is_never_updated() {
    let root = scratch("foreign-update");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "forgejo_update_pull_request",
            &serde_json::json!({
                "repository": REPO,
                "number": 3,
                "title": "Not mine"
            })
            .to_string(),
        ),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([pull_json(
        3,
        "someone-else"
    )])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-foreign-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    assert!(
        forgejo
            .requests()
            .iter()
            .all(|request| request.method != "PATCH"),
        "the product patched another account's request"
    );
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("another account's request")
            && transcript.contains("updates only its own open request"),
        "{transcript}"
    );
    assert!(gewu_multiverse::run::forgejo_actions(&run_dir).is_empty());
}

#[test]
fn a_duplicate_open_keeps_the_refusal_and_names_the_existing_request() {
    let root = scratch("duplicate");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "forgejo_open_pull_request",
            &serde_json::json!({"repository": REPO, "title": "Another one"}).to_string(),
        ),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([pull_json(
        3,
        "gewu-scribe"
    )])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-duplicate-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    assert!(
        forgejo
            .requests()
            .iter()
            .all(|request| !(request.method == "POST" && request.path.ends_with("/pulls"))),
        "the product opened a duplicate"
    );
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("is already open") && transcript.contains("lab/analysis-code#3"),
        "{transcript}"
    );
    let actions = gewu_multiverse::run::forgejo_actions(&run_dir);
    assert!(
        actions.iter().any(
            |action| action.sentence.contains("#3") && action.sentence.contains("already open")
        ),
        "the existing request is not named: {actions:?}"
    );
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let outcome = match terminal {
        Record::RunFinished { outcome, .. } => outcome,
        other => panic!("no terminal record: {other:?}"),
    };
    assert_eq!(
        outcome, "refused",
        "with no other work, the agent's already-open request keeps the refusal mark"
    );
    assert!(
        gewu_multiverse::run::work_summary(&run_dir).is_empty(),
        "an already-open request alone is not this run's work"
    );
}

#[test]
fn a_push_then_an_existing_open_request_is_a_success_that_names_the_request() {
    // Walk A F3: the run pushed a corrective commit, then opening the pull request
    // found the agent's own request already open. The push is real work; the mark
    // must stay a success and the work line must name the request, not only the
    // branch.
    let root = scratch("push-existing-pr");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "shell",
            &serde_json::json!({
                "command": "cd analysis-code && git commit --allow-empty -q -m 'second' && git push -q origin feature"
            })
            .to_string(),
        ),
        tool_reply(
            "c2",
            "forgejo_open_pull_request",
            &serde_json::json!({"repository": REPO, "title": "Another one"}).to_string(),
        ),
        final_reply("Opened the pull request for the README wording fix."),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([pull_json(
        3,
        "gewu-scribe"
    )])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-push-existing-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());

    assert!(
        run_dir.join(gewu_multiverse::run::PUSH_FILE).is_file(),
        "the push witness is missing"
    );
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let outcome = match terminal {
        Record::RunFinished { outcome, .. } => outcome,
        other => panic!("no terminal record: {other:?}"),
    };
    assert_eq!(
        outcome, "succeeded",
        "a push plus the agent's own open request is not a refusal"
    );
    assert_eq!(
        gewu_multiverse::run::work_summary(&run_dir),
        vec!["pull request #3 from feature".to_owned()],
        "the work line did not name the pull request"
    );
}

#[test]
fn a_comment_survives_a_turn_limit() {
    // A real comment is recorded before the run hits its turn bound. The mark is
    // failed, but the work line must keep the product-observed comment.
    let root = scratch("comment-limit");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![tool_reply(
        "c1",
        "forgejo_comment",
        &serde_json::json!({"repository": REPO, "number": 12, "body": "a comment"}).to_string(),
    )]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-comment-limit-1", 1);
    write_context(&run_dir, &forgejo);
    let _ = run_binary(&run_dir);
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    assert_eq!(
        outcome, "failed",
        "the turn limit is still a failed run: {reason}"
    );
    assert_eq!(
        gewu_multiverse::run::work_summary(&run_dir),
        vec!["comment on #12".to_owned()],
        "the recorded comment was lost on the early exit"
    );
}

#[test]
fn a_comment_survives_a_later_refused_update_as_success() {
    // Real remote work outranks a later refused write: the comment is on Forgejo,
    // so the run is a success and its work line names the comment, not a refusal.
    let root = scratch("comment-then-refused");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "forgejo_comment",
            &serde_json::json!({"repository": REPO, "number": 12, "body": "a comment"}).to_string(),
        ),
        tool_reply(
            "c2",
            "forgejo_update_pull_request",
            &serde_json::json!({"repository": REPO, "number": 3, "title": "Not now"}).to_string(),
        ),
        final_reply("I could not update the pull request."),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-comment-refused-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let outcome = match terminal {
        Record::RunFinished { outcome, .. } => outcome,
        other => panic!("no terminal record: {other:?}"),
    };
    assert_eq!(
        outcome, "succeeded",
        "the recorded comment is real work even though the update was refused"
    );
    assert_eq!(
        gewu_multiverse::run::work_summary(&run_dir),
        vec!["comment on #12".to_owned()]
    );
}

#[test]
fn a_read_only_check_is_no_work_and_keeps_its_sentence() {
    // Walk B F2 through the built-in loop: list and read the assigned issue, write
    // nothing, and end with the run's own no-work sentence. The recorded read fact
    // is what decides the mark; the instruction's prose does not.
    let root = scratch("read-only");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply("c1", "forgejo_list_assigned_issues", "{}"),
        tool_reply(
            "c2",
            "forgejo_read_issue_or_pull",
            &serde_json::json!({"repository": REPO, "number": 12}).to_string(),
        ),
        final_reply("no work — I read issue #12 and nothing needed changing."),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-read-only-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let work = gewu_multiverse::run::work_summary(&run_dir);
    assert_eq!(
        outcome, "no_work",
        "a read-only check is no work, not a command failure: {reason}"
    );
    // The built-in loop keeps the model's first clause as the card reason; the
    // native supervisor keeps the whole sentence (covered in native-local-outcome).
    assert_eq!(reason, "no work");
    assert!(work.is_empty(), "a read is not work: {work:?}");
}

#[test]
fn a_repository_outside_the_workspace_is_refused_with_one_actionable_sentence() {
    let root = scratch("outside");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply(
            "c1",
            "forgejo_comment",
            &serde_json::json!({"repository": "lab/other", "number": 1, "body": "no"}).to_string(),
        ),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-outside-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    assert!(
        forgejo
            .requests()
            .iter()
            .all(|request| request.method != "POST"),
        "a request was sent for a repository outside the workspace"
    );
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("`lab/other` is not cloned in this agent's workspace")
            && transcript.contains("`lab/analysis-code`"),
        "{transcript}"
    );
}

#[test]
fn the_per_run_write_bound_refuses_the_ninth_write() {
    let root = scratch("bound");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let mut replies = Vec::new();
    for index in 0..9 {
        replies.push(tool_reply(
            &format!("c{index}"),
            "forgejo_comment",
            &serde_json::json!({"repository": REPO, "number": 12, "body": format!("comment {index}")})
                .to_string(),
        ));
    }
    replies.push(final_reply("done"));
    let model = Stub::start(replies);
    let forgejo = Stub::start_routed(standard_routes(serde_json::json!([])));
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-bound-1", 12);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let posts = forgejo
        .requests()
        .iter()
        .filter(|request| request.method == "POST")
        .count();
    assert_eq!(posts, 8, "the write bound did not stop the ninth write");
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("write bound of 8 is reached"),
        "{transcript}"
    );
}

/// B2: a write whose 2xx reply cannot be parsed still happened, so it counts.
///
/// The first accepted comment answers `201 text/plain`; the product reports
/// `malformed_reply` for it but must use up one of the eight writes. Nine comment
/// calls therefore send eight POSTs, never nine, and the ninth call is refused before
/// it reaches the instance.
#[test]
fn an_accepted_write_with_an_unparseable_reply_counts_against_the_bound() {
    let root = scratch("bound-unparseable");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let mut replies = Vec::new();
    for index in 0..9 {
        replies.push(tool_reply(
            &format!("c{index}"),
            "forgejo_comment",
            &serde_json::json!({"repository": REPO, "number": 12, "body": format!("comment {index}")})
                .to_string(),
        ));
    }
    replies.push(final_reply("done"));
    let model = Stub::start(replies);
    let json_comment = StubReply::status(
        201,
        serde_json::json!({
            "id": 31,
            "body": "a comment",
            "html_url": format!("http://forgejo.test/{REPO}/issues/12#issuecomment-31"),
            "user": {"login": "gewu-scribe"},
            "created_at": "2026-01-02T03:04:05Z"
        })
        .to_string(),
    );
    // The first write is accepted with a body that is not JSON; every later write is
    // accepted with the expected shape.
    let forgejo = Stub::start(vec![
        StubReply::status(201, "accepted, but this is not JSON"),
        json_comment,
    ]);
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-bound-unparseable-1", 12);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let posts = forgejo
        .requests()
        .iter()
        .filter(|request| request.method == "POST")
        .count();
    assert_eq!(
        posts, 8,
        "a 2xx write whose reply could not be parsed did not count against the bound"
    );
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("malformed_reply"),
        "the unparseable reply was not reported: {transcript}"
    );
    assert!(
        transcript.contains("write bound of 8 is reached"),
        "{transcript}"
    );
}

#[test]
fn a_missing_scope_names_the_right_and_the_model_never_sees_the_token() {
    let root = scratch("scope");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![
        tool_reply("c1", "forgejo_list_assigned_issues", "{}"),
        final_reply("done"),
    ]);
    let forgejo = Stub::start_routed(vec![Route::new(
        "GET",
        "/api/v1/repos/issues/search",
        StubReply::status(
            403,
            r#"{"message":"token does not have at least one of required scope(s): [read:issue]"}"#,
        ),
    )]);
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-scope-1", 6);
    write_context(&run_dir, &forgejo);
    assert!(run_binary(&run_dir).success());
    let transcript = transcript_text(&run_dir);
    assert!(
        transcript.contains("missing the Forgejo rights read:issue"),
        "{transcript}"
    );
    for request in model.requests() {
        assert!(!request.body.contains(AGENT_TOKEN), "{}", request.body);
    }
}

#[test]
fn a_run_without_a_forgejo_context_declares_shell_alone() {
    let root = scratch("shell-only");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![final_reply("done")]);
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-shell-1", 4);
    assert!(run_binary(&run_dir).success());
    let requests = model.requests();
    let body: serde_json::Value =
        serde_json::from_str(&requests[0].body).expect("the model request is JSON");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["function"]["name"], "shell");
}

/// A declared shell step sees a path to the credentials file, never the token itself;
/// the run's records and workspace keep no token either.
#[test]
fn the_shell_environment_holds_no_agent_token() {
    let root = scratch("shell-env");
    write_credentials(&root.join("credentials.toml"));
    let _ = clone_fixture(&root);
    let model = Stub::start(vec![final_reply("done")]);
    let forgejo = Stub::start_routed(Vec::new());
    let layout = Layout::rooted_at(root.clone()).expect("the test layout is absolute");
    let paths = layout.agent("scribe").expect("the agent path");
    let run_dir = write_plan(&root, &paths, &model, "run-env-1", 4);
    write_context(&run_dir, &forgejo);
    let mut plan = RunPlan::read(&run_dir).expect("the plan");
    plan.steps = vec!["env".to_owned()];
    plan.write(&run_dir).expect("the plan with a step");
    assert!(run_binary(&run_dir).success());
    let log = log_text(&run_dir);
    assert!(log.contains("GEWU_GIT_CREDENTIALS"), "{log}");
    assert!(
        !log.contains(AGENT_TOKEN),
        "the shell environment holds the token"
    );
    assert!(
        !transcript_text(&run_dir).contains(AGENT_TOKEN),
        "the transcript holds the token"
    );
}
