//! A native run's local-only change is a warning that names the local work.
//!
//! The program is a fixture that speaks Pi's JSON event shape: it edits one file in
//! its workspace and reports a clean finish. The product's own supervisor, workspace
//! comparison and terminal record decide the mark; no real model or Forgejo is used.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::mode;
use gewu_multiverse::run::{self, NativePlan, RunPlan};
use gewu_multiverse::transcript::{self, Record};

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-native-local-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&root).expect("the scratch root");
    root
}

fn fake_pi(install_root: &Path) -> PathBuf {
    let path = install_root.join("pi").join("fake-pi");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/bin/sh
while IFS= read -r line; do :; done
printf 'a local edit\n' > README.md
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"I edited README.md locally."}]}}'
printf '%s\n' '{"type":"agent_settled","aborted":false}'
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

#[test]
fn a_native_local_only_change_is_warned_and_names_the_file() {
    let root = scratch("pi");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    fs::create_dir_all(&workspace).expect("the workspace");
    fs::create_dir_all(&skills).expect("the skills");
    fs::write(root.join("instruction.md"), "Edit the README and report.\n").expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-native-local");
    fs::create_dir_all(&run_dir).expect("the run directory");
    RunPlan {
        run_id: "run-native-local".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    // The real product binary is the supervisor here: the native child re-execs
    // `current_exe() --confined-program`, which only the product binary understands.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let local = run::read_local_work(&run_dir).expect("the local work record");
    let work = run::work_summary(&run_dir);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(outcome, "warned", "{reason}");
    assert!(reason.contains("README.md"), "{reason}");
    assert!(local.files.contains(&"README.md".to_owned()), "{local:?}");
    assert!(
        work.iter().any(|line| line.contains("README.md")),
        "the work line did not name the local file: {work:?}"
    );
}

fn fake_pi_reader(install_root: &Path) -> PathBuf {
    // A Pi-shaped program that reads its issue through the product's own MCP bridge
    // and writes nothing. The supervisor's ledger is the only record of the read.
    let path = install_root.join("pi").join("fake-pi-reader");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json, os, socket, sys, time

sys.stdin.read()
root = os.environ["PI_CODING_AGENT_DIR"]
with open(os.path.join(root, "mcp.json")) as handle:
    config = json.load(handle)
socket_path = config["mcpServers"]["gewu_forgejo"]["args"][1]
stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(200):
    try:
        stream.connect(socket_path)
        break
    except OSError:
        time.sleep(0.02)
reader = stream.makefile("rb")

def call(number, name, arguments):
    request = {"jsonrpc": "2.0", "id": number, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}}
    stream.sendall((json.dumps(request) + "\n").encode())
    return json.loads(reader.readline())

initialize = {"jsonrpc": "2.0", "id": 0, "method": "initialize",
              "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                         "clientInfo": {"name": "fixture", "version": "1"}}}
stream.sendall((json.dumps(initialize) + "\n").encode())
json.loads(reader.readline())
call(1, "forgejo_list_assigned_issues", {})
call(2, "forgejo_read_issue_or_pull", {"repository": "team/workspace", "number": 12})
stream.close()
print(json.dumps({"type": "message_end", "message": {
    "role": "assistant", "stopReason": "stop", "content": [
        {"type": "text", "text": "no work — I read issue #12 and nothing needed changing."}]}}))
print(json.dumps({"type": "agent_settled", "aborted": False}))
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

fn fake_pi_opener(install_root: &Path) -> PathBuf {
    // A Pi-shaped program that calls the product's MCP bridge to open the request for
    // its current branch, then reports a clean finish.
    let path = install_root.join("pi").join("fake-pi-opener");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json, os, socket, sys, time

sys.stdin.read()
root = os.environ["PI_CODING_AGENT_DIR"]
with open(os.path.join(root, "mcp.json")) as handle:
    config = json.load(handle)
socket_path = config["mcpServers"]["gewu_forgejo"]["args"][1]
stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(200):
    try:
        stream.connect(socket_path)
        break
    except OSError:
        time.sleep(0.02)
reader = stream.makefile("rb")

def send(request):
    stream.sendall((json.dumps(request) + "\n").encode())
    return json.loads(reader.readline())

send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "fixture", "version": "1"}}})
send({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
      "params": {"name": "forgejo_open_pull_request", "arguments": {
          "repository": "team/workspace", "title": "A pull request"}}})
stream.close()
print(json.dumps({"type": "message_end", "message": {
    "role": "assistant", "stopReason": "stop", "content": [
        {"type": "text", "text": "Opened the pull request for the feature branch."}]}}))
print(json.dumps({"type": "agent_settled", "aborted": False}))
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

fn fake_pi_pusher(install_root: &Path) -> PathBuf {
    // A Pi-shaped program that pushes a commit and then asks the bridge to open the
    // request. The request is already open, so the push is the run's real work.
    let path = install_root.join("pi").join("fake-pi-pusher");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json, os, socket, subprocess, sys, time

sys.stdin.read()
clone = os.path.join(os.getcwd(), "workspace")
subprocess.run(["git", "commit", "--allow-empty", "-q", "-m", "second"], cwd=clone, check=True)
subprocess.run(["git", "push", "-q", "origin", "feature"], cwd=clone, check=True)
root = os.environ["PI_CODING_AGENT_DIR"]
with open(os.path.join(root, "mcp.json")) as handle:
    config = json.load(handle)
socket_path = config["mcpServers"]["gewu_forgejo"]["args"][1]
stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(200):
    try:
        stream.connect(socket_path)
        break
    except OSError:
        time.sleep(0.02)
reader = stream.makefile("rb")

def send(request):
    stream.sendall((json.dumps(request) + "\n").encode())
    return json.loads(reader.readline())

send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "fixture", "version": "1"}}})
send({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
      "params": {"name": "forgejo_open_pull_request", "arguments": {
          "repository": "team/workspace", "title": "A pull request"}}})
stream.close()
print(json.dumps({"type": "message_end", "message": {
    "role": "assistant", "stopReason": "stop", "content": [
        {"type": "text", "text": "Opened the pull request for the README wording fix."}]}}))
print(json.dumps({"type": "agent_settled", "aborted": False}))
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

#[test]
fn a_native_read_only_check_is_no_work_with_its_own_sentence() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};

    let root = scratch("pi-reader");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    let clone = workspace.join("workspace");
    fs::create_dir_all(clone.join(".git")).expect("the clone");
    fs::create_dir_all(&skills).expect("the skills");
    fs::write(
        root.join("instruction.md"),
        "Work on the local Forgejo. Read the open issues, pick the oldest one and do what it asks. When done, comment on the issue with a short summary of the change or say what you checked. If there is nothing to do, say so and stop.\n",
    )
    .expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\n[forgejo_agent_tokens]\nscribe = \"gw-fixture-agent-token\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-native-reader");
    fs::create_dir_all(&run_dir).expect("the run directory");
    let issue = serde_json::json!({
        "number": 12,
        "title": "An assigned issue",
        "body": "the body",
        "state": "open",
        "html_url": "http://forgejo.test/team/workspace/issues/12",
        "labels": [],
        "updated_at": "2026-01-02T03:04:05Z",
        "user": {"login": "owner"},
        "comments": 0,
        "repository": {"full_name": "team/workspace"},
    });
    let forgejo = Stub::start_routed(vec![
        Route::new(
            "GET",
            "/api/v1/repos/issues/search",
            StubReply::json(serde_json::json!([issue]).to_string()),
        ),
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace/issues/12",
            StubReply::json(issue.to_string()),
        ),
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace/issues/12/comments",
            StubReply::json("[]".to_owned()),
        ),
    ]);
    fs::write(
        run_dir.join("forgejo.json"),
        serde_json::json!({
            "url": format!("http://{}", forgejo.addr()),
            "login": "gewu-scribe",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}],
        })
        .to_string(),
    )
    .expect("the Forgejo context");
    RunPlan {
        run_id: "run-native-reader".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_reader(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        // The MCP socket lives below the run's private temp root. A short temp root
        // keeps the Unix socket path inside SUN_LEN on this task's longer workdir.
        .env("TMPDIR", "/tmp")
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        outcome, "no_work",
        "a read-only check is no work, not a command failure: {reason}"
    );
    assert_eq!(
        reason,
        "no work — I read issue #12 and nothing needed changing."
    );
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn clone_fixture(workspace: &Path) {
    // The bare remote lives inside the workspace, below `.git`, so the confined
    // native program can read and write it with the same grant as the clone. The
    // workspace snapshot ignores `.git` internals, so the push is not local work.
    let remote = workspace.join(".git").join("remote.git");
    git(
        workspace,
        &["init", "-q", "--bare", "-b", "main", ".git/remote.git"],
    );
    let destination = workspace.join("workspace");
    std::fs::create_dir_all(&destination).expect("the clone directory");
    git(
        workspace,
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
    std::fs::write(destination.join("README.md"), "hello\n").expect("a file");
    git(&destination, &["add", "README.md"]);
    git(&destination, &["commit", "-qm", "first"]);
    git(&destination, &["push", "-q", "-u", "origin", "HEAD"]);
    git(&destination, &["checkout", "-q", "-b", "feature"]);
    git(
        &destination,
        &["commit", "-q", "--allow-empty", "-m", "work"],
    );
    git(&destination, &["push", "-q", "-u", "origin", "feature"]);
}

#[test]
fn a_native_opened_request_is_named_in_the_work_line() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};

    let root = scratch("pi-pr");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    std::fs::create_dir_all(&workspace).expect("the workspace");
    std::fs::create_dir_all(&skills).expect("the skills");
    clone_fixture(&workspace);
    std::fs::write(
        root.join("instruction.md"),
        "Open the pull request for the checked-out branch.\n",
    )
    .expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\n[forgejo_agent_tokens]\nscribe = \"gw-fixture-agent-token\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-native-pr");
    std::fs::create_dir_all(&run_dir).expect("the run directory");
    let opened = serde_json::json!({
        "number": 6,
        "title": "A pull request",
        "state": "open",
        "html_url": "http://forgejo.test/team/workspace/pulls/6",
        "head": {"ref": "feature", "repo": {"full_name": "team/workspace"}},
        "base": {"ref": "main", "repo": {"full_name": "team/workspace"}},
        "user": {"login": "gewu-scribe"},
    });
    let forgejo = Stub::start_routed(vec![
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace/pulls",
            StubReply::json("[]".to_owned()),
        ),
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace",
            StubReply::json(
                serde_json::json!({
                    "full_name": "team/workspace",
                    "name": "workspace",
                    "default_branch": "main",
                })
                .to_string(),
            ),
        ),
        Route::new(
            "POST",
            "/api/v1/repos/team/workspace/pulls",
            StubReply::status(201, opened.to_string()),
        ),
    ]);
    std::fs::write(
        run_dir.join("forgejo.json"),
        serde_json::json!({
            "url": format!("http://{}", forgejo.addr()),
            "login": "gewu-scribe",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}],
        })
        .to_string(),
    )
    .expect("the Forgejo context");
    RunPlan {
        run_id: "run-native-pr".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_opener(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .env("TMPDIR", "/tmp")
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let work = run::work_summary(&run_dir);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        outcome, "succeeded",
        "the agent's own open request is real work: {reason}"
    );
    assert_eq!(
        work,
        vec!["pull request #6 from feature".to_owned()],
        "the work line did not name the opened request"
    );
}

#[test]
fn a_native_push_then_an_existing_request_is_a_success_that_names_the_request() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};

    let root = scratch("pi-push-pr");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    std::fs::create_dir_all(&workspace).expect("the workspace");
    std::fs::create_dir_all(&skills).expect("the skills");
    clone_fixture(&workspace);
    std::fs::write(
        root.join("instruction.md"),
        "Push the checked-out branch and open its pull request.\n",
    )
    .expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\n[forgejo_agent_tokens]\nscribe = \"gw-fixture-agent-token\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-native-push-pr");
    std::fs::create_dir_all(&run_dir).expect("the run directory");
    let pull = serde_json::json!({
        "number": 3,
        "title": "A pull request",
        "state": "open",
        "html_url": "http://forgejo.test/team/workspace/pulls/3",
        "head": {"ref": "feature", "repo": {"full_name": "team/workspace"}},
        "base": {"ref": "main", "repo": {"full_name": "team/workspace"}},
        "user": {"login": "gewu-scribe"},
    });
    let forgejo = Stub::start_routed(vec![
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace/pulls",
            StubReply::json(serde_json::json!([pull]).to_string()),
        ),
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace",
            StubReply::json(
                serde_json::json!({
                    "full_name": "team/workspace",
                    "name": "workspace",
                    "default_branch": "main",
                })
                .to_string(),
            ),
        ),
    ]);
    std::fs::write(
        run_dir.join("forgejo.json"),
        serde_json::json!({
            "url": format!("http://{}", forgejo.addr()),
            "login": "gewu-scribe",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}],
        })
        .to_string(),
    )
    .expect("the Forgejo context");
    RunPlan {
        run_id: "run-native-push-pr".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_pusher(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .env("TMPDIR", "/tmp")
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let work = run::work_summary(&run_dir);
    let pushed = run::read_push_witness(&run_dir).is_some();
    let _ = std::fs::remove_dir_all(&root);
    assert!(pushed, "the fixture push was not recorded");
    assert_eq!(
        outcome, "succeeded",
        "the push is real work even though the request was already open: {reason}"
    );
    assert_eq!(
        work,
        vec!["pull request #3 from feature".to_owned()],
        "the work line did not name the request"
    );
}

fn fake_pi_script(install_root: &Path, tag: &str, body: &str) -> PathBuf {
    let path = install_root.join("pi").join(format!("fake-pi-{tag}"));
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    let script = format!(
        r#"#!/usr/bin/python3
import json, os, sys

sys.stdin.read()
{body}
print(json.dumps({{"type": "message_end", "message": {{
    "role": "assistant", "stopReason": "stop", "content": [
        {{"type": "text", "text": "Done: the work is complete."}}]}}}}))
print(json.dumps({{"type": "agent_settled", "aborted": False}}))
"#
    );
    fs::write(&path, script).expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

fn run_pi_script(
    tag: &str,
    body: &str,
) -> (
    String,
    String,
    Vec<String>,
    Option<gewu_multiverse::workspace::WorkspaceChanges>,
) {
    let root = scratch(tag);
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    fs::create_dir_all(&workspace).expect("the workspace");
    fs::create_dir_all(&skills).expect("the skills");
    fs::write(workspace.join("README.md"), "hello\n").expect("the README");
    fs::write(root.join("instruction.md"), "Fix the README and report.\n").expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join(format!("run-{tag}"));
    fs::create_dir_all(&run_dir).expect("the run directory");
    RunPlan {
        run_id: format!("run-{tag}"),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_script(&install_root, tag, body),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .status()
        .expect("the product runs the native run");
    assert!(
        status.success(),
        "{tag}: the supervisor exited with {status:?}"
    );
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let local = run::read_local_work(&run_dir);
    let work = run::work_summary(&run_dir);
    let _ = fs::remove_dir_all(&root);
    (outcome, reason, work, local)
}

#[test]
fn a_native_run_with_no_recorded_fact_is_unconfirmed() {
    let (outcome, reason, work, local) = run_pi_script("no-fact", "pass");
    assert_eq!(outcome, "warned", "{reason}");
    assert!(
        reason.contains("no tool call or workspace change"),
        "{reason}"
    );
    assert!(local.is_none(), "{local:?}");
    assert!(work.is_empty(), "{work:?}");
}

#[test]
fn a_native_workspace_change_the_old_snapshot_missed_is_local_work() {
    let cases = [
        ("chmod", "os.chmod(\"README.md\", 0o755)", "README.md"),
        ("symlink", "os.symlink(\"README.md\", \"link\")", "link"),
        ("empty-dir", "os.mkdir(\"empty\")", "empty"),
        (
            "same-length-same-time",
            "st = os.stat(\"README.md\")\nwith open(\"README.md\", \"w\") as handle:\n    handle.write(\"world\\n\")\nos.utime(\"README.md\", ns=(st.st_atime_ns, st.st_mtime_ns))",
            "README.md",
        ),
    ];
    for (tag, body, named) in cases {
        let (outcome, reason, work, local) = run_pi_script(tag, body);
        let local = local.expect("the local work record");
        assert_eq!(outcome, "warned", "{tag}: {reason}");
        assert!(
            local.files.iter().any(|file| file == named),
            "{tag}: {local:?}"
        );
        assert!(
            work.iter().any(|line| line.contains(named)),
            "{tag}: the work line does not name {named}: {work:?}"
        );
        assert!(reason.contains(named), "{tag}: {reason}");
    }
}

fn fake_pi_local_opener(install_root: &Path) -> PathBuf {
    // Edit the workspace, then find the agent's own request already open. The
    // request is not this run's work; the local edit is.
    let path = install_root.join("pi").join("fake-pi-local-opener");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json, os, socket, sys, time

sys.stdin.read()
with open("README.md", "a") as handle:
    handle.write("local edit\n")
root = os.environ["PI_CODING_AGENT_DIR"]
with open(os.path.join(root, "mcp.json")) as handle:
    config = json.load(handle)
socket_path = config["mcpServers"]["gewu_forgejo"]["args"][1]
stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(200):
    try:
        stream.connect(socket_path)
        break
    except OSError:
        time.sleep(0.02)
reader = stream.makefile("rb")

def send(request):
    stream.sendall((json.dumps(request) + "\n").encode())
    return json.loads(reader.readline())

send({"jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": {"name": "fixture", "version": "1"}}})
send({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
      "params": {"name": "forgejo_open_pull_request", "arguments": {
          "repository": "team/workspace", "title": "A pull request"}}})
stream.close()
print(json.dumps({"type": "message_end", "message": {
    "role": "assistant", "stopReason": "stop", "content": [
        {"type": "text", "text": "The pull request is open."}]}}))
print(json.dumps({"type": "agent_settled", "aborted": False}))
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

#[test]
fn a_native_existing_request_does_not_replace_this_runs_local_work() {
    use gewu_multiverse::test_stub::{Route, Stub, StubReply};

    let root = scratch("pi-local-existing");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    std::fs::create_dir_all(&workspace).expect("the workspace");
    std::fs::create_dir_all(&skills).expect("the skills");
    clone_fixture(&workspace);
    std::fs::write(workspace.join("README.md"), "hello\n").expect("the README");
    std::fs::write(
        root.join("instruction.md"),
        "Fix the README and open its pull request.\n",
    )
    .expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\n[forgejo_agent_tokens]\nscribe = \"gw-fixture-agent-token\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-local-existing");
    std::fs::create_dir_all(&run_dir).expect("the run directory");
    let pull = serde_json::json!({
        "number": 3,
        "title": "A pull request",
        "state": "open",
        "html_url": "http://forgejo.test/team/workspace/pulls/3",
        "head": {"ref": "feature", "repo": {"full_name": "team/workspace"}},
        "base": {"ref": "main", "repo": {"full_name": "team/workspace"}},
        "user": {"login": "gewu-scribe"},
    });
    let forgejo = Stub::start_routed(vec![
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace/pulls",
            StubReply::json(serde_json::json!([pull]).to_string()),
        ),
        Route::new(
            "GET",
            "/api/v1/repos/team/workspace",
            StubReply::json(
                serde_json::json!({
                    "full_name": "team/workspace",
                    "name": "workspace",
                    "default_branch": "main",
                })
                .to_string(),
            ),
        ),
    ]);
    std::fs::write(
        run_dir.join("forgejo.json"),
        serde_json::json!({
            "url": format!("http://{}", forgejo.addr()),
            "login": "gewu-scribe",
            "repositories": [{"full_name": "team/workspace", "directory": "workspace"}],
        })
        .to_string(),
    )
    .expect("the Forgejo context");
    RunPlan {
        run_id: "run-local-existing".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_local_opener(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .env("TMPDIR", "/tmp")
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let work = run::work_summary(&run_dir);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        outcome, "refused",
        "the already-open no-op keeps its refusal: {reason}"
    );
    assert_eq!(
        work,
        vec!["local change README.md".to_owned()],
        "the work line named a request this run did not write"
    );
}

fn fake_pi_fetch(install_root: &Path) -> PathBuf {
    let path = install_root.join("pi").join("fake-pi-fetch");
    fs::create_dir_all(path.parent().expect("the install directory"))
        .expect("the install directory");
    fs::write(
        &path,
        r#"#!/usr/bin/python3
import json, os, subprocess, sys

sys.stdin.read()
subprocess.run(["git", "fetch", "origin"], cwd=os.path.join(os.getcwd(), "workspace"), check=True)
print(json.dumps({"type": "tool_execution_start", "toolCallId": "fetch1",
                  "toolName": "bash", "args": {"command": "git fetch origin"}}))
print(json.dumps({"type": "tool_execution_end", "toolCallId": "fetch1",
                  "toolName": "bash", "result": {"content": [{"type": "text", "text": "fetched"}],
                  "structuredContent": {"exit_code": 0}}}))
print(json.dumps({"type": "message_end", "message": {
    "role": "assistant", "stopReason": "stop", "content": [
        {"type": "text", "text": "Done: checked the remote."}]}}))
print(json.dumps({"type": "agent_settled", "aborted": False}))
"#,
    )
    .expect("the fixture program");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    path
}

#[test]
fn a_native_fetch_is_no_work_not_a_local_git_change() {
    let root = scratch("pi-fetch");
    let workspace = root.join("workspace");
    let skills = root.join("skills");
    fs::create_dir_all(&workspace).expect("the workspace");
    fs::create_dir_all(&skills).expect("the skills");
    clone_fixture(&workspace);
    fs::write(
        root.join("instruction.md"),
        "Fetch the remote and report.\n",
    )
    .expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-native-fetch");
    fs::create_dir_all(&run_dir).expect("the run directory");
    RunPlan {
        run_id: "run-native-fetch".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace.clone(),
        instruction: root.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "landlock".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan");
    let install_root = root.join("install");
    NativePlan {
        harness: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        program: fake_pi_fetch(&install_root),
        install_root,
        protocol: "chat-completions".to_owned(),
        base_url: "http://127.0.0.1:1/v1".to_owned(),
        model: "fixture-model".to_owned(),
        codex_sandbox: String::new(),
    }
    .write(&run_dir)
    .expect("the native plan");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .status()
        .expect("the product runs the native run");
    assert!(status.success(), "the supervisor exited with {status:?}");
    let terminal = transcript::terminal_record(&run_dir).expect("the terminal record");
    let (outcome, reason) = match terminal {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    let work = run::work_summary(&run_dir);
    let local = run::read_local_work(&run_dir);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        outcome, "no_work",
        "a read-only fetch is not a local change: {reason}"
    );
    assert!(work.is_empty(), "a fetch is not work: {work:?}");
    assert!(local.is_none(), "a fetch was recorded as local: {local:?}");
}

#[test]
fn an_unrelated_existing_request_is_not_work() {
    use gewu_multiverse::run::{RunFacts, WorkPull};

    let facts = RunFacts {
        remote: true,
        comments: vec![5],
        existing: vec![WorkPull {
            number: 3,
            branch: Some("feature".to_owned()),
        }],
        ..RunFacts::default()
    };
    assert_eq!(
        facts.work_lines(),
        vec!["comment on #5".to_owned()],
        "an already-open request from another branch became this run's work"
    );
}
