//! Live proof that the product launches real pinned native programs through its own
//! confinement, configuration and MCP bridge. This is `#[ignore]`d because it needs
//! the official Forgejo binary; run it with:
//!
//!   GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test native-programs -- --ignored --nocapture
//!
//! The model endpoint is a scripted loopback stub. It is labelled `synthetic-model`
//! everywhere; no paid provider is contacted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::credentials;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{Command, EngineApi, ForgejoState, Provisioning};
use gewu_multiverse::forgejo::{DEFAULT_TIMEOUT, ForgejoClient};
use gewu_multiverse::harness::{self, InstallRecord, Program};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::test_stub::{Stub, StubReply};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("gewu-native-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("the scratch root is made");
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("GEWU_KEEP_NATIVE").is_some() {
            eprintln!("kept native scratch: {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_engine(
    engine: &mut impl EngineApi,
    what: &str,
    limit: Duration,
    mut ready: impl FnMut(&mut dyn EngineApi) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < limit {
        engine.tick();
        if ready(engine) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let board = engine.board();
    eprintln!(
        "timed out waiting for {what}: forgejo={:?} events={:?} notice={:?} agents={:?}",
        board.forgejo.state,
        board.forgejo.events,
        board.notice,
        board.agents.len()
    );
    panic!("timed out waiting for {what}");
}

fn install_pi_fixture(data_root: &Path) {
    let source =
        std::env::var("GEWU_PI_BINARY").unwrap_or_else(|_| panic!("GEWU_PI_BINARY is not set"));
    let source = PathBuf::from(source);
    assert!(source.is_file(), "{} is not a file", source.display());
    let root = data_root.join("harnesses/pi/1.4.0");
    fs::create_dir_all(root.join("pi")).expect("the fixture directory");
    let target = root.join("pi/pi");
    if !target.exists() {
        fs::hard_link(&source, &target)
            .or_else(|_| fs::copy(&source, &target).map(|_| ()))
            .expect("the fixture binary");
    }
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("the fixture mode");
    let record = InstallRecord {
        program: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        sha256: "f2e4e4329603223d03260cdb8bd5033dbdc1f73ba58ab8939db2e56546e115d2".to_owned(),
        license: "MIT".to_owned(),
        entrypoint: "pi/pi".to_owned(),
        resources: None,
        installed_at: "2026-10-23T00:00:00Z".to_owned(),
    };
    let text = serde_json::to_string_pretty(&record).expect("the record");
    gewu_multiverse::mode::write_private(root.join("install.json"), text.as_bytes())
        .expect("the install record");
    assert!(harness::installed(data_root, Program::Pi).is_some());
}

fn hardlink_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            hardlink_tree(&from, &to)?;
        } else if kind.is_symlink() {
            let destination = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(destination, &to)?;
        } else {
            std::fs::hard_link(&from, &to).or_else(|_| std::fs::copy(&from, &to).map(|_| ()))?;
        }
    }
    Ok(())
}

fn install_fixture(data_root: &Path, program: Program, source: &Path, entrypoint: &str) {
    let root = data_root
        .join("harnesses")
        .join(program.id())
        .join(program.pinned_version());
    if !root.join("install.json").is_file() {
        hardlink_tree(source, &root).expect("the fixture tree");
        let record = InstallRecord {
            program: program.id().to_owned(),
            version: program.pinned_version().to_owned(),
            sha256: "fixture".to_owned(),
            license: program.license().to_owned(),
            entrypoint: entrypoint.to_owned(),
            resources: None,
            installed_at: "2026-10-23T00:00:00Z".to_owned(),
        };
        let text = serde_json::to_string_pretty(&record).expect("the record");
        gewu_multiverse::mode::write_private(root.join("install.json"), text.as_bytes())
            .expect("the install record");
    }
    let executable = root.join(entrypoint);
    let _ = std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755));
    assert!(harness::installed(data_root, program).is_some());
}

fn sse_codex_tool(name: &str, namespace: &str, arguments: serde_json::Value) -> StubReply {
    let item = serde_json::json!({
        "type": "function_call",
        "id": "fc_native",
        "call_id": "call_native",
        "name": name,
        "namespace": namespace,
        "arguments": arguments.to_string()
    });
    let created =
        serde_json::json!({"type": "response.created", "response": {"id": "resp_native"}});
    let done = serde_json::json!({"type": "response.output_item.done", "item": item});
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {"id": "resp_native", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
    });
    StubReply::sse(format!(
        "{}{}{}data: [DONE]\n\n",
        sse_event(created),
        sse_event(done),
        sse_event(completed)
    ))
}

fn sse_codex_final(text: &str) -> StubReply {
    let created =
        serde_json::json!({"type": "response.created", "response": {"id": "resp_native_done"}});
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "item": {"type": "message", "id": "msg_native_done", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
    });
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {"id": "resp_native_done", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
    });
    StubReply::sse(format!(
        "{}{}{}data: [DONE]\n\n",
        sse_event(created),
        sse_event(done),
        sse_event(completed)
    ))
}

fn sse_event(value: serde_json::Value) -> String {
    format!("data: {value}\n\n")
}

fn sse_tool(id: &str, name: &str, arguments: serde_json::Value) -> StubReply {
    let first = serde_json::json!({
        "id": "chatcmpl-native",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": null, "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]},
            "finish_reason": null
        }]
    });
    let done = serde_json::json!({
        "id": "chatcmpl-native",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    StubReply::sse(format!(
        "{}{}data: [DONE]\n\n",
        sse_event(first),
        sse_event(done)
    ))
}

fn sse_final(text: &str) -> StubReply {
    let first = serde_json::json!({
        "id": "chatcmpl-native-done",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    });
    let done = serde_json::json!({
        "id": "chatcmpl-native-done",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    StubReply::sse(format!(
        "{}{}data: [DONE]\n\n",
        sse_event(first),
        sse_event(done)
    ))
}

#[test]
#[ignore = "live Forgejo + a real Pi binary: run with GEWU_FORGEJO_BINARY=... GEWU_PI_BINARY=... cargo test --test native-programs -- --ignored --nocapture"]
fn native_pi_reads_an_issue_pushes_a_branch_and_opens_a_pull_request() {
    let forgejo = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(forgejo.is_file(), "{} is not a file", forgejo.display());
    assert_eq!(
        bundled_forgejo::sha256_file(&forgejo).expect("the binary hashes"),
        bundled_forgejo::SHA256,
        "the staged Forgejo binary is not the pinned one"
    );

    let root = Scratch::new("pi-live");
    let layout = Layout::rooted_at(root.0.join("config")).expect("an absolute layout");
    let data_root = root.0.join("data");
    install_pi_fixture(&data_root);
    let product = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the bundled Forgejo",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let url = engine.board().forgejo.url.clone();
    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("the credentials file reads")
        .expect("the administrator token is recorded");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue(
            "team",
            "workspace",
            "Native Pi proof",
            "Read this, push a branch and open a pull request.",
        )
        .expect("the issue is opened");
    eprintln!("live: opened team/workspace#{} at {url}", issue.number);

    let branch = "native-pi-r12";
    let model = Stub::start(vec![
        sse_tool(
            "read",
            "mcp__gewu_forgejo__forgejo_read_issue_or_pull",
            serde_json::json!({"repository": "team/workspace", "number": issue.number}),
        ),
        sse_tool(
            "push",
            "bash",
            serde_json::json!({
                "command": format!(
                    "cd workspace && git checkout -b {branch} && \
                     printf 'native pi r12\\n' > native-pi.txt && \
                     git add native-pi.txt && \
                     git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 \
                         commit -m 'native pi r12' -q && \
                     git push -u origin {branch} && echo push_ok"
                )
            }),
        ),
        sse_tool(
            "pr",
            "mcp__gewu_forgejo__forgejo_open_pull_request",
            serde_json::json!({
                "repository": "team/workspace",
                "title": "Native Pi r12",
                "body": format!("The native Pi agent pushed this for issue #{}.", issue.number)
            }),
        ),
        sse_tool(
            "comment",
            "mcp__gewu_forgejo__forgejo_comment",
            serde_json::json!({
                "repository": "team/workspace",
                "number": issue.number,
                "body": format!("Native Pi worked issue #{}", issue.number)
            }),
        ),
        sse_final("The branch is pushed and the pull request is open."),
    ]);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-native-live-key".to_owned()),
        })
        .expect("the synthetic endpoint is saved");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(
        &mut engine,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    let board = engine.board();
    assert_eq!(
        board.agents[0].program, "pi",
        "the fresh install default is not Pi"
    );
    assert!(board.agents[0].program_installed);
    assert_eq!(board.agents[0].protocol, "chat-completions");
    assert!(
        board.confined,
        "the host is unconfined; this test needs the product Landlock boundary"
    );
    fs::write(
        engine
            .instruction_path("agent-1")
            .expect("the instruction path"),
        format!(
            "Read team/workspace#{}, push branch `{branch}` and open a pull request, then comment on the issue.\n",
            issue.number
        ),
    )
    .expect("the instruction is written");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 starts");
    let run_dir = wait_for_run(&mut engine, &layout, "agent-1", Duration::from_secs(60));
    let transcript = fs::read_to_string(run_dir.join("transcript.jsonl")).unwrap_or_default();
    eprintln!("live: transcript\n{transcript}");
    assert!(
        transcript.contains("\"event\":\"native_started\""),
        "the run did not record a native start"
    );
    assert!(
        transcript.contains("mcp__gewu_forgejo__forgejo_read_issue_or_pull")
            || transcript.contains("forgejo_read_issue_or_pull"),
        "the model did not call the read tool"
    );
    assert!(
        transcript.contains("\"event\":\"forgejo_result\"")
            && transcript.contains("forgejo_open_pull_request"),
        "the MCP write was not recorded"
    );
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir)
        .expect("the run has a terminal record");
    match terminal {
        gewu_multiverse::transcript::Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, "succeeded", "{reason:?}");
            assert!(
                reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("pull request"),
                "the final summary is not the native final answer: {reason:?}"
            );
        }
        other => panic!("unexpected terminal record {other:?}"),
    }
    let pulls = admin_client
        .pull_requests("team", "workspace")
        .expect("the pull requests are readable");
    assert!(
        pulls.iter().any(|pull| pull.head == branch),
        "the native Pi did not open a pull request from {branch}: {pulls:?}"
    );
    let comments = admin_client
        .issue_comments("team", "workspace", issue.number, 20)
        .expect("the comments are readable");
    assert!(
        comments
            .iter()
            .any(|comment| comment.body.contains("Native Pi worked")),
        "the native Pi did not comment as the agent: {comments:?}"
    );

    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 stops");
    engine.shutdown(Duration::from_secs(10));
}

fn wait_for_run(
    _engine: &mut impl EngineApi,
    layout: &Layout,
    agent: &str,
    limit: Duration,
) -> PathBuf {
    let start = Instant::now();
    let state = layout
        .agent(agent)
        .expect("the agent layout")
        .state_dir()
        .to_path_buf();
    while start.elapsed() < limit {
        _engine.tick();
        if let Ok(entries) = fs::read_dir(&state) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if let Some(record) = gewu_multiverse::transcript::terminal_record(&dir) {
                    if matches!(
                        record,
                        gewu_multiverse::transcript::Record::RunFinished { .. }
                    ) {
                        return dir;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for the first native run");
}

#[test]
#[ignore = "live Forgejo + a real Codex binary: run with GEWU_FORGEJO_BINARY=... GEWU_CODEX_ROOT=... cargo test --test native-programs native_codex -- --ignored --nocapture"]
fn native_codex_reads_an_issue_pushes_and_opens_a_pull_request() {
    let forgejo = PathBuf::from(
        std::env::var("GEWU_FORGEJO_BINARY")
            .unwrap_or_else(|_| panic!("GEWU_FORGEJO_BINARY is not set")),
    );
    assert_eq!(
        bundled_forgejo::sha256_file(&forgejo).expect("the binary hashes"),
        bundled_forgejo::SHA256
    );
    let source = PathBuf::from(
        std::env::var("GEWU_CODEX_ROOT").unwrap_or_else(|_| panic!("GEWU_CODEX_ROOT is not set")),
    );
    let root = Scratch::new("codex-live");
    let layout = Layout::rooted_at(root.0.join("config")).expect("an absolute layout");
    let data_root = root.0.join("data");
    install_fixture(&data_root, Program::Codex, &source, "bin/codex");
    let product = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the bundled Forgejo",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let url = engine.board().forgejo.url.clone();
    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("the credentials file reads")
        .expect("the administrator token is recorded");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue(
            "team",
            "workspace",
            "Native Codex proof",
            "Do the native work.",
        )
        .expect("the issue is opened");
    let branch = "native-codex-r12";
    let model = Stub::start(vec![
        sse_codex_tool(
            "forgejo_read_issue_or_pull",
            "mcp__gewu_forgejo",
            serde_json::json!({"repository": "team/workspace", "number": issue.number}),
        ),
        sse_codex_tool(
            "exec_command",
            "functions",
            serde_json::json!({
                "cmd": format!(
                    "cd workspace && git checkout -b {branch} && printf 'native codex r12\\n' > native-codex.txt && git add native-codex.txt && git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'native codex r12' -q && git push -u origin {branch} && echo push_ok"
                )
            }),
        ),
        sse_codex_tool(
            "forgejo_open_pull_request",
            "mcp__gewu_forgejo",
            serde_json::json!({
                "repository": "team/workspace",
                "title": "Native Codex r12",
                "body": format!("The native Codex agent pushed this for issue #{}.", issue.number)
            }),
        ),
        sse_codex_tool(
            "forgejo_comment",
            "mcp__gewu_forgejo",
            serde_json::json!({
                "repository": "team/workspace",
                "number": issue.number,
                "body": format!("Native Codex worked issue #{}", issue.number)
            }),
        ),
        sse_codex_final("Codex pushed the branch and opened the pull request."),
    ]);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-native-codex-key".to_owned()),
        })
        .expect("the synthetic endpoint is saved");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(
        &mut engine,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "codex".to_owned(),
            protocol: Some("responses".to_owned()),
        })
        .expect("Codex is selected");
    engine
        .send(Command::SetAgentCodexSandbox {
            name: "agent-1".to_owned(),
            mode: "full-access".to_owned(),
        })
        .expect("the D106 opt-in is saved");
    wait_engine(
        &mut engine,
        "Codex to be selected",
        Duration::from_secs(30),
        |engine| {
            let board = engine.board();
            board.agents.first().is_some_and(|agent| {
                agent.program == "codex"
                    && agent.protocol == "responses"
                    && agent.codex_sandbox.as_deref() == Some("full-access")
            })
        },
    );
    assert!(engine.board().confined);
    fs::write(
        engine
            .instruction_path("agent-1")
            .expect("the instruction path"),
        format!(
            "Read team/workspace#{}, push `{branch}`, open a pull request and comment.\n",
            issue.number
        ),
    )
    .expect("the instruction is written");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 starts");
    let run_dir = wait_for_run(&mut engine, &layout, "agent-1", Duration::from_secs(240));
    let transcript = fs::read_to_string(run_dir.join("transcript.jsonl")).unwrap_or_default();
    eprintln!("live Codex transcript\n{transcript}");
    for expected in [
        "native_started",
        "forgejo_read_issue_or_pull",
        "\"tool\":\"shell\"",
        "forgejo_open_pull_request",
        "forgejo_comment",
    ] {
        assert!(
            transcript.contains(expected),
            "missing {expected} in the transcript"
        );
    }
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir).expect("terminal");
    match terminal {
        gewu_multiverse::transcript::Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, "succeeded", "{reason:?}");
            assert!(reason.as_deref().unwrap_or_default().contains("Codex"));
        }
        other => panic!("unexpected terminal record {other:?}"),
    }
    let pulls = admin_client
        .pull_requests("team", "workspace")
        .expect("the pulls");
    assert!(pulls.iter().any(|pull| pull.head == branch));
    let comments = admin_client
        .issue_comments("team", "workspace", issue.number, 20)
        .expect("the comments");
    assert!(
        comments
            .iter()
            .any(|comment| comment.body.contains("Native Codex worked"))
    );
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 stops");
    engine.shutdown(Duration::from_secs(10));
}

#[test]
#[ignore = "live Forgejo + a real DSH binary: run with GEWU_FORGEJO_BINARY=... GEWU_DSH_ROOT=... cargo test --test native-programs native_dsh -- --ignored --nocapture"]
fn native_dsh_reads_an_issue_pushes_and_opens_a_pull_request() {
    let forgejo = PathBuf::from(
        std::env::var("GEWU_FORGEJO_BINARY")
            .unwrap_or_else(|_| panic!("GEWU_FORGEJO_BINARY is not set")),
    );
    assert_eq!(
        bundled_forgejo::sha256_file(&forgejo).expect("the binary hashes"),
        bundled_forgejo::SHA256
    );
    let source = PathBuf::from(
        std::env::var("GEWU_DSH_ROOT").unwrap_or_else(|_| panic!("GEWU_DSH_ROOT is not set")),
    );
    let root = Scratch::new("dsh-live");
    let layout = Layout::rooted_at(root.0.join("config")).expect("an absolute layout");
    let data_root = root.0.join("data");
    install_fixture(&data_root, Program::Dsh, &source, "node_modules/.bin/dsh");
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(&mut engine, "Forgejo", Duration::from_secs(240), |engine| {
        let board = engine.board();
        matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
    });
    let url = engine.board().forgejo.url.clone();
    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("credentials")
        .expect("admin token");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue(
            "team",
            "workspace",
            "Native DSH proof",
            "Do the native work.",
        )
        .expect("issue");
    let branch = "native-dsh-r12";
    let model = Stub::start(vec![
        sse_tool(
            "read",
            "mcp__gewu_forgejo__forgejo_read_issue_or_pull",
            serde_json::json!({"repository": "team/workspace", "number": issue.number}),
        ),
        sse_tool(
            "push",
            "bash",
            serde_json::json!({
                "command": format!(
                    "cd workspace && git checkout -b {branch} && printf 'native dsh r12\\n' > native-dsh.txt && git add native-dsh.txt && git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'native dsh r12' -q && git push -u origin {branch} && echo push_ok"
                ),
                "description": "push the native DSH branch"
            }),
        ),
        sse_tool(
            "pr",
            "mcp__gewu_forgejo__forgejo_open_pull_request",
            serde_json::json!({
                "repository": "team/workspace",
                "title": "Native DSH r12",
                "body": format!("The native DSH agent pushed this for issue #{}.", issue.number)
            }),
        ),
        sse_tool(
            "comment",
            "mcp__gewu_forgejo__forgejo_comment",
            serde_json::json!({
                "repository": "team/workspace",
                "number": issue.number,
                "body": format!("Native DSH worked issue #{}", issue.number)
            }),
        ),
        sse_final("DSH pushed the branch and opened the pull request."),
    ]);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-native-dsh-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    wait_engine(
        &mut engine,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "dsh".to_owned(),
            protocol: Some("chat-completions".to_owned()),
        })
        .expect("DSH is selected");
    wait_engine(
        &mut engine,
        "DSH to be selected",
        Duration::from_secs(30),
        |engine| {
            engine
                .board()
                .agents
                .first()
                .is_some_and(|agent| agent.program == "dsh")
        },
    );
    assert!(engine.board().confined);
    fs::write(
        engine.instruction_path("agent-1").expect("instruction"),
        format!(
            "Read team/workspace#{}, push `{branch}`, open a pull request and comment.\n",
            issue.number
        ),
    )
    .expect("instruction written");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    let run_dir = wait_for_run(&mut engine, &layout, "agent-1", Duration::from_secs(240));
    let transcript = fs::read_to_string(run_dir.join("transcript.jsonl")).unwrap_or_default();
    eprintln!("live DSH transcript\n{transcript}");
    for expected in [
        "native_started",
        "forgejo_read_issue_or_pull",
        "\"tool\":\"bash\"",
        "forgejo_open_pull_request",
        "forgejo_comment",
    ] {
        assert!(transcript.contains(expected), "missing {expected}");
    }
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir).expect("terminal");
    match terminal {
        gewu_multiverse::transcript::Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, "succeeded", "{reason:?}");
        }
        other => panic!("unexpected terminal record {other:?}"),
    }
    let pulls = admin_client
        .pull_requests("team", "workspace")
        .expect("pulls");
    assert!(pulls.iter().any(|pull| pull.head == branch));
    let comments = admin_client
        .issue_comments("team", "workspace", issue.number, 20)
        .expect("comments");
    assert!(
        comments
            .iter()
            .any(|comment| comment.body.contains("Native DSH worked"))
    );
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop");
    engine.shutdown(Duration::from_secs(10));
}

#[test]
#[ignore = "live Forgejo + a real OpenCode binary: run with GEWU_FORGEJO_BINARY=... GEWU_OPENCODE_ROOT=... cargo test --test native-programs native_opencode -- --ignored --nocapture"]
fn native_opencode_runs_its_own_shell_tool_inside_the_product_boundary() {
    let forgejo = PathBuf::from(
        std::env::var("GEWU_FORGEJO_BINARY")
            .unwrap_or_else(|_| panic!("GEWU_FORGEJO_BINARY is not set")),
    );
    assert_eq!(
        bundled_forgejo::sha256_file(&forgejo).expect("the binary hashes"),
        bundled_forgejo::SHA256
    );
    let source = PathBuf::from(
        std::env::var("GEWU_OPENCODE_ROOT")
            .unwrap_or_else(|_| panic!("GEWU_OPENCODE_ROOT is not set")),
    );
    let root = Scratch::new("opencode-live");
    let layout = Layout::rooted_at(root.0.join("config")).expect("an absolute layout");
    let data_root = root.0.join("data");
    install_fixture(
        &data_root,
        Program::Opencode,
        &source,
        "node_modules/@opencode/cli-linux-x64/bin/opencode",
    );
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(&mut engine, "Forgejo", Duration::from_secs(240), |engine| {
        let board = engine.board();
        matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
    });
    let model = Stub::start(vec![
        sse_tool(
            "shell",
            "shell",
            serde_json::json!({"command": "printf 'native opencode r12\\n' > native-opencode.txt"}),
        ),
        sse_tool(
            "shell",
            "shell",
            serde_json::json!({"command": "printf 'native opencode r12\\n' > native-opencode.txt"}),
        ),
        sse_final("OpenCode wrote the file with its own shell tool."),
    ]);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-native-opencode-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    wait_engine(
        &mut engine,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "opencode".to_owned(),
            protocol: Some("chat-completions".to_owned()),
        })
        .expect("OpenCode is selected");
    wait_engine(
        &mut engine,
        "OpenCode to be selected",
        Duration::from_secs(30),
        |engine| {
            engine
                .board()
                .agents
                .first()
                .is_some_and(|agent| agent.program == "opencode")
        },
    );
    assert!(engine.board().confined);
    fs::write(
        engine.instruction_path("agent-1").expect("instruction"),
        "Use your own shell tool to write native-opencode.txt, then finish.\n",
    )
    .expect("instruction written");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    let run_dir = wait_for_run(&mut engine, &layout, "agent-1", Duration::from_secs(240));
    let transcript = fs::read_to_string(run_dir.join("transcript.jsonl")).unwrap_or_default();
    eprintln!("live OpenCode transcript\n{transcript}");
    assert!(transcript.contains("\"tool\":\"shell\""), "{transcript}");
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir).expect("terminal");
    match terminal {
        gewu_multiverse::transcript::Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, "succeeded", "{reason:?}");
        }
        other => panic!("unexpected terminal record {other:?}"),
    }
    let proof = layout
        .agent("agent-1")
        .expect("agent layout")
        .workspace()
        .join("native-opencode.txt");
    assert_eq!(
        fs::read_to_string(&proof).expect("the native OpenCode write"),
        "native opencode r12\n"
    );
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop");
    engine.shutdown(Duration::from_secs(10));
}
