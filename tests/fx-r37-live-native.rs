//! Wave 37 live material: install the four official programs, then run one real
//! native run per program for C1 (work/no-work) and C5 (a 503 that recovers).
//!
//!   GEWU_W37_DATA=/task/data cargo test --test fx-r37-live-native install_wave37 -- --ignored --nocapture
//!   GEWU_W37_DATA=/task/data GEWU_W37_HOME=/task/live-home \
//!     cargo test --test fx-r37-live-native wave37_c1_c5_runs -- --ignored --nocapture
//!
//! No real provider is contacted: every model request goes to the task's loopback
//! `Stub`. The programs themselves are the official pinned packages. The engine,
//! Forgejo, agent accounts, workspaces, MCP bridge and run records are the product's.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gewu_multiverse::credentials;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{
    Command, EngineApi, ForgejoState, Provisioning, SessionEnd, SessionState,
};
use gewu_multiverse::forgejo::{DEFAULT_TIMEOUT, ForgejoClient};
use gewu_multiverse::harness;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use serde_json::json;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn program_protocol(program: &str) -> &'static str {
    if program == "codex" {
        "responses"
    } else {
        "chat-completions"
    }
}

#[test]
#[ignore = "downloads and installs the four official pinned programs once"]
fn install_wave37_harnesses() {
    let data = env("GEWU_W37_DATA");
    let data = Path::new(&data);
    for program in harness::NATIVE_PROGRAMS {
        if let Some(installed) = harness::installed(data, program) {
            eprintln!(
                "wave37: {} {} already installed at {}",
                program.label(),
                installed.version,
                installed.root.display()
            );
            continue;
        }
        eprintln!("wave37: installing {} ...", program.label());
        let installed = harness::install(data, program, |_percent| {})
            .unwrap_or_else(|error| panic!("{} could not be installed: {error}", program.label()));
        eprintln!(
            "wave37: {} {} at {}",
            program.label(),
            installed.version,
            installed.root.display()
        );
    }
}

fn sse_event(value: serde_json::Value) -> String {
    format!("data: {value}\n\n")
}

fn sse_tool(id: &str, name: &str, arguments: serde_json::Value) -> StubReply {
    let first = json!({
        "id": "chatcmpl-w37",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": null, "tool_calls": [{
                "index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]},
            "finish_reason": null
        }]
    });
    let done = json!({
        "id": "chatcmpl-w37",
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
    let first = json!({
        "id": "chatcmpl-w37-done",
        "object": "chat.completion.chunk",
        "model": "synthetic-model",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
    });
    let done = json!({
        "id": "chatcmpl-w37-done",
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

fn sse_codex_tool(name: &str, namespace: &str, arguments: serde_json::Value) -> StubReply {
    let item = json!({
        "type": "function_call",
        "id": "fc_w37",
        "call_id": "call_w37",
        "name": name,
        "namespace": namespace,
        "arguments": arguments.to_string()
    });
    let created = json!({"type": "response.created", "response": {"id": "resp_w37"}});
    let done = json!({"type": "response.output_item.done", "item": item});
    let completed = json!({
        "type": "response.completed",
        "response": {"id": "resp_w37", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
    });
    StubReply::sse(format!(
        "{}{}{}data: [DONE]\n\n",
        sse_event(created),
        sse_event(done),
        sse_event(completed)
    ))
}

fn sse_codex_final(text: &str) -> StubReply {
    let created = json!({"type": "response.created", "response": {"id": "resp_w37_done"}});
    let done = json!({
        "type": "response.output_item.done",
        "item": {"type": "message", "id": "msg_w37_done", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
    });
    let completed = json!({
        "type": "response.completed",
        "response": {"id": "resp_w37_done", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
    });
    StubReply::sse(format!(
        "{}{}{}data: [DONE]\n\n",
        sse_event(created),
        sse_event(done),
        sse_event(completed)
    ))
}

fn chat_read(program: &str, issue: u64) -> StubReply {
    match program {
        "pi" | "dsh" => sse_tool(
            "read",
            "mcp__gewu_forgejo__forgejo_read_issue_or_pull",
            json!({"repository": "team/workspace", "number": issue}),
        ),
        "opencode" => sse_tool(
            "read",
            "execute",
            json!({"code": format!("const r = await tools.gewu_forgejo.forgejo_read_issue_or_pull({{repository: \"team/workspace\", number: {issue}}}); return r;")}),
        ),
        _ => unreachable!(),
    }
}

fn codex_read(issue: u64) -> StubReply {
    sse_codex_tool(
        "forgejo_read_issue_or_pull",
        "mcp__gewu_forgejo",
        json!({"repository": "team/workspace", "number": issue}),
    )
}

fn chat_comment(program: &str, issue: u64, body: &str) -> StubReply {
    match program {
        "pi" | "dsh" => sse_tool(
            "comment",
            "mcp__gewu_forgejo__forgejo_comment",
            json!({"repository": "team/workspace", "number": issue, "body": body}),
        ),
        "opencode" => sse_tool(
            "comment",
            "execute",
            json!({"code": format!("const r = await tools.gewu_forgejo.forgejo_comment({{repository: \"team/workspace\", number: {issue}, body: {body:?}}}); return r;")}),
        ),
        _ => unreachable!(),
    }
}

fn codex_comment(issue: u64, body: &str) -> StubReply {
    sse_codex_tool(
        "forgejo_comment",
        "mcp__gewu_forgejo",
        json!({"repository": "team/workspace", "number": issue, "body": body}),
    )
}

fn chat_pr(program: &str, branch: &str, issue: u64) -> StubReply {
    match program {
        "pi" | "dsh" => sse_tool(
            "pr",
            "mcp__gewu_forgejo__forgejo_open_pull_request",
            json!({"repository": "team/workspace", "title": format!("Wave 37 {branch}"), "body": format!("The native run for issue #{issue}.")}),
        ),
        "opencode" => sse_tool(
            "pr",
            "execute",
            json!({"code": format!("const r = await tools.gewu_forgejo.forgejo_open_pull_request({{repository: \"team/workspace\", title: \"Wave 37 {branch}\", body: \"The native run for issue #{issue}.\"}}); return r;")}),
        ),
        _ => unreachable!(),
    }
}

fn codex_pr(branch: &str, issue: u64) -> StubReply {
    sse_codex_tool(
        "forgejo_open_pull_request",
        "mcp__gewu_forgejo",
        json!({"repository": "team/workspace", "title": format!("Wave 37 {branch}"), "body": format!("The native run for issue #{issue}.")}),
    )
}

fn command_for(program: &str, command: &str) -> StubReply {
    match program {
        "pi" => sse_tool("shell", "bash", json!({"command": command})),
        "dsh" => sse_tool(
            "shell",
            "bash",
            json!({"command": command, "description": "run the wave 37 shell step"}),
        ),
        "opencode" => sse_tool("shell", "shell", json!({"command": command})),
        "codex" => sse_codex_tool("exec_command", "functions", json!({"cmd": command})),
        _ => unreachable!(),
    }
}

fn final_for(program: &str, text: &str) -> StubReply {
    match program {
        "codex" => sse_codex_final(text),
        _ => sse_final(text),
    }
}

fn read_for(program: &str, issue: u64) -> StubReply {
    match program {
        "codex" => codex_read(issue),
        _ => chat_read(program, issue),
    }
}

fn comment_for(program: &str, issue: u64, body: &str) -> StubReply {
    match program {
        "codex" => codex_comment(issue, body),
        _ => chat_comment(program, issue, body),
    }
}

fn pr_for(program: &str, branch: &str, issue: u64) -> StubReply {
    match program {
        "codex" => codex_pr(branch, issue),
        _ => chat_pr(program, branch, issue),
    }
}

fn delayed(mut reply: StubReply, delay: Duration) -> StubReply {
    reply.delay = delay;
    reply
}

fn sequence(program: &str, issue: u64, scenario: &str, branch: &str) -> Vec<StubReply> {
    match scenario {
        "comment-push" => vec![
            command_for(
                program,
                &format!(
                    "cd workspace && git checkout -b {branch} && printf 'w37 {scenario}\\n' > {branch}.txt && git add {branch}.txt && git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'wave37 {scenario}' -q && git push -u origin {branch} && echo pushed"
                ),
            ),
            comment_for(program, issue, &format!("Wave 37 {program} commented after a push.")),
            final_for(program, "The branch is pushed and the issue has a comment."),
        ],
        "no-work" => vec![
            read_for(program, issue),
            final_for(program, "nothing new to add"),
        ],
        "failing-pr" => vec![
            command_for(
                program,
                "cd workspace && python3 -c 'import sys; sys.exit(1)'",
            ),
            command_for(
                program,
                &format!(
                    "cd workspace && git branch {branch} HEAD; git checkout -b {branch} 2>/dev/null; git switch {branch} && printf 'w37 fixed\\n' > {branch}.txt && git add {branch}.txt && git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'wave37 fix' -q && git push -u origin {branch} && echo pushed"
                ),
            ),
            pr_for(program, branch, issue),
            final_for(
                program,
                "I could not finish the task. The work is still pending; please check it.",
            ),
        ],
        "local-failed-push" => vec![
            command_for(
                program,
                &format!(
                    "cd workspace && git checkout -b {branch} && printf 'local only\n' > {branch}.txt && git add {branch}.txt && git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'local only' -q && git push no-such-remote HEAD"
                ),
            ),
            final_for(
                program,
                "I could not finish the task. The work is still pending; please check it.",
            ),
        ],
        "503-recovers" => vec![
            StubReply::status(
                503,
                json!({"error": {"message": "The model is temporarily unavailable. Retry later.", "type": "server_error"}})
                    .to_string(),
            ),
            delayed(read_for(program, issue), Duration::from_secs(2)),
            final_for(program, "The model recovered and the issue was read."),
        ],
        _ => unreachable!("unknown scenario {scenario}"),
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
    panic!("timed out waiting for {what}: {:?}", engine.board());
}

fn newest_run(layout: &Layout, agent: &str) -> Option<PathBuf> {
    let state = layout.agent(agent).ok()?.state_dir().to_path_buf();
    fs::read_dir(state)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run-"))
        })
        .max()
}

fn terminal(run_dir: &Path) -> (String, String) {
    match transcript::terminal_record(run_dir).expect("a terminal record") {
        Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    }
}

struct Observed {
    outcome: String,
    reason: String,
    run_dir: PathBuf,
    retrying: Option<String>,
    board: Option<SessionEnd>,
}

fn run_scenario(
    engine: &mut impl EngineApi,
    layout: &Layout,
    index: usize,
    program: &str,
    issue: u64,
    scenario: &str,
    stub: &Stub,
) -> Observed {
    let name = format!("agent-{}", index + 1);
    let protocol = program_protocol(program);
    engine
        .send(Command::SetAgentModel {
            name: name.clone(),
            base_url: stub.url().trim_end_matches("/chat/completions").to_owned(),
            protocol: protocol.to_owned(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-wave37-live-key".to_owned()),
        })
        .expect("the agent's loopback connection");
    let instruction = match scenario {
        "comment-push" => format!(
            "Read issue #{issue} in team/workspace, push branch `w37-{program}-comment`, and comment on the issue. Do not open a pull request."
        ),
        "no-work" => format!(
            "Read issue #{issue} in team/workspace and report whether anything needs doing. Do not change any files or write anything."
        ),
        "failing-pr" => format!(
            "Run the test, then fix it on branch `w37-{program}-pr`, push it, and open a pull request for issue #{issue}."
        ),
        "local-failed-push" => format!(
            "Make a local commit on branch `w37-{program}-local`, then push it. Do not open a pull request."
        ),
        "503-recovers" => format!(
            "Read issue #{issue} in team/workspace. The model may be temporarily unavailable; if so, retry. Do not change anything."
        ),
        _ => unreachable!(),
    };
    fs::write(
        engine.instruction_path(&name).expect("instruction path"),
        instruction,
    )
    .expect("instruction");
    engine.tick();
    let before = engine.board().agents[index].runs_total;
    engine
        .send(Command::Start { name: name.clone() })
        .expect("start");

    let start = Instant::now();
    let mut retrying = None;
    let run_dir = loop {
        engine.tick();
        let board = engine.board();
        let agent = &board.agents[index];
        if let Some(cause) = agent.retrying.clone() {
            retrying = Some(cause);
        }
        if agent.runs_total > before
            && let Some(dir) = newest_run(layout, &name)
            && transcript::terminal_record(&dir).is_some()
        {
            break dir;
        }
        if start.elapsed() > Duration::from_secs(240) {
            panic!(
                "timed out waiting for {program}/{scenario}: {:?}",
                engine.board().agents[index]
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (outcome, reason) = terminal(&run_dir);
    // The terminal record is written by the child; the engine settles it on a later
    // tick. Wait for the card to carry this run's mark before reading the board.
    let expected = match scenario {
        "no-work" => SessionEnd::NoWork {
            reason: String::new(),
        },
        "local-failed-push" => SessionEnd::Warned {
            reason: String::new(),
        },
        _ => SessionEnd::Succeeded,
    };
    let mark_matches = |end: &SessionEnd| {
        matches!(
            (&expected, end),
            (SessionEnd::Succeeded, SessionEnd::Succeeded)
                | (SessionEnd::NoWork { .. }, SessionEnd::NoWork { .. })
                | (SessionEnd::Warned { .. }, SessionEnd::Warned { .. })
        )
    };
    let settle = Instant::now();
    let board = loop {
        engine.tick();
        let board = engine.board();
        let agent = &board.agents[index];
        let settled = !matches!(agent.session, SessionState::Running { .. })
            && agent.recent.last().is_some_and(mark_matches);
        if settled {
            break board;
        }
        if settle.elapsed() > Duration::from_secs(20) {
            panic!("the board did not settle {program}/{scenario}: {:?}", agent);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let agent = &board.agents[index];
    eprintln!(
        "wave37-live: {program:>8} {scenario:>12} requests={:?}",
        stub.requests()
            .iter()
            .map(|request| (request.method.clone(), request.path.clone()))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "wave37-live: {program:>8} {scenario:>12} run={} outcome={outcome} board={:?} reason={reason}",
        run_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?"),
        agent.recent.last()
    );
    let board_mark = agent.recent.last().cloned();
    engine
        .send(Command::Stop { name: name.clone() })
        .expect("stop");
    wait_engine(
        engine,
        "the agent to stop",
        Duration::from_secs(20),
        |engine| {
            !matches!(
                engine.board().agents[index].session,
                SessionState::Running { .. }
            )
        },
    );
    Observed {
        outcome,
        reason,
        run_dir,
        retrying,
        board: board_mark,
    }
}

#[test]
#[ignore = "live Forgejo + the four official programs; needs GEWU_W37_DATA and GEWU_W37_HOME"]
fn wave37_c1_c5_runs() {
    let data = PathBuf::from(env("GEWU_W37_DATA"));
    let home = PathBuf::from(env("GEWU_W37_HOME"));
    let layout = Layout::rooted_at(home.join(".config/gewu-multiverse")).expect("layout");
    layout.ensure_root().expect("config root");
    let mut options = EngineOptions::new(layout.clone(), &data).expect("options");
    options.product_binary = std::env::var("GEWU_W37_PRODUCT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse")));
    options.auto_start_forgejo = true;
    options.ready_timeout = Duration::from_secs(240);
    options.stop_grace = Duration::from_secs(3);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetConnection {
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-wave37-live-key".to_owned()),
        })
        .expect("default connection");
    engine
        .send(Command::CreateAgents { count: 4 })
        .expect("four agents");
    wait_engine(
        &mut engine,
        "Forgejo and the four agent accounts",
        Duration::from_secs(300),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running)
                && board.forgejo.repositories >= 1
                && board.agents.len() == 4
                && board
                    .agents
                    .iter()
                    .all(|agent| agent.provisioning == Provisioning::Ready)
        },
    );
    let url = engine.board().forgejo.url.clone();
    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("credentials")
        .expect("admin token");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue(
            "team",
            "workspace",
            "Wave 37 native scenarios",
            "Read this issue. The task instruction names the work.",
        )
        .expect("the issue");
    eprintln!("wave37-live: issue #{} at {url}", issue.number);

    let programs = ["pi", "codex", "dsh", "opencode"];
    for (index, program) in programs.iter().enumerate() {
        let name = format!("agent-{}", index + 1);
        engine
            .send(Command::SetAgentProgram {
                name: name.clone(),
                program: (*program).to_owned(),
                protocol: Some(program_protocol(program).to_owned()),
            })
            .expect("program");
        if *program == "codex" {
            engine
                .send(Command::SetAgentCodexSandbox {
                    name: name.clone(),
                    mode: "full-access".to_owned(),
                })
                .expect("the tested Codex compatibility opt-out");
        }
        wait_engine(
            &mut engine,
            &format!("{program} to be selected"),
            Duration::from_secs(30),
            |engine| {
                engine.board().agents[index].program == *program
                    && engine.board().agents[index].program_installed
            },
        );
    }

    let only = std::env::var("GEWU_W37_ONLY").unwrap_or_default();
    for (index, program) in programs.iter().enumerate() {
        if !only.is_empty() && !only.contains(program) {
            continue;
        }
        for scenario in [
            "comment-push",
            "no-work",
            "failing-pr",
            "503-recovers",
            "local-failed-push",
        ] {
            if !only.is_empty() && !format!("{program}/{scenario}").contains(&only) {
                continue;
            }
            let branch = match scenario {
                "comment-push" => format!("w37-{program}-comment"),
                "failing-pr" => format!("w37-{program}-pr"),
                "local-failed-push" => format!("w37-{program}-local"),
                _ => String::new(),
            };
            let replies = sequence(program, issue.number, scenario, &branch);
            let stub = if *program == "opencode" {
                Stub::start_with_title(replies, sse_final("Wave 37 session"))
            } else {
                Stub::start(replies)
            };
            let observed = run_scenario(
                &mut engine,
                &layout,
                index,
                program,
                issue.number,
                scenario,
                &stub,
            );
            let transcript_text =
                fs::read_to_string(observed.run_dir.join("transcript.jsonl")).unwrap_or_default();
            match scenario {
                "comment-push" | "failing-pr" => {
                    assert_eq!(
                        observed.outcome, "succeeded",
                        "{program}/{scenario}: {}",
                        observed.reason
                    );
                    assert!(
                        matches!(observed.board, Some(SessionEnd::Succeeded)),
                        "{program}/{scenario}: the card mark is {:?}",
                        observed.board
                    );
                    assert!(
                        observed.run_dir.join("push.json").is_file(),
                        "{program}/{scenario}: no push witness"
                    );
                    if scenario == "comment-push" {
                        assert!(
                            transcript_text.contains("\"action\":\"commented\""),
                            "{program}/{scenario}: no comment result\n{transcript_text}"
                        );
                    }
                }
                "local-failed-push" => {
                    assert_eq!(
                        observed.outcome, "warned",
                        "{program}/{scenario}: {}",
                        observed.reason
                    );
                    assert!(
                        matches!(observed.board, Some(SessionEnd::Warned { .. })),
                        "{program}/{scenario}: the card mark is {:?}",
                        observed.board
                    );
                    assert!(
                        observed.reason.contains("workspace changed"),
                        "{program}/{scenario}: the reason denied the local work: {}",
                        observed.reason
                    );
                    assert!(
                        observed.reason.contains("failed to push")
                            || observed.reason.contains("no-such-remote")
                            || observed
                                .reason
                                .contains("does not appear to be a git repository")
                            || observed
                                .reason
                                .contains("Could not read from remote repository"),
                        "{program}/{scenario}: the failed push is missing: {}",
                        observed.reason
                    );
                    assert!(
                        !observed.run_dir.join("push.json").is_file(),
                        "{program}/{scenario}: a failed push was recorded as a push"
                    );
                }
                "no-work" => {
                    assert_eq!(
                        observed.outcome, "no_work",
                        "{program}/{scenario}: {}",
                        observed.reason
                    );
                    assert!(
                        matches!(observed.board, Some(SessionEnd::NoWork { .. })),
                        "{program}/{scenario}: the card mark is {:?}",
                        observed.board
                    );
                    assert!(
                        !observed.run_dir.join("push.json").is_file(),
                        "{program}/{scenario}: a read-only cycle pushed"
                    );
                }
                "503-recovers" => {
                    assert_eq!(
                        observed.outcome, "succeeded",
                        "{program}/{scenario}: {}",
                        observed.reason
                    );
                    assert!(
                        matches!(observed.board, Some(SessionEnd::Succeeded)),
                        "{program}/{scenario}: the card mark is {:?}",
                        observed.board
                    );
                    let retrying = observed.retrying.as_deref().unwrap_or("");
                    if matches!(*program, "dsh" | "opencode") && retrying.is_empty() {
                        // These two programs retry inside their own process without a
                        // status event or a log line. The run is still the proof that
                        // the 503 recovered; the card shows working until it does.
                        eprintln!(
                            "wave37-live: {program}/503 recovered without a retry signal (program limitation)"
                        );
                    } else {
                        assert!(
                            retrying.contains("503"),
                            "{program}/{scenario}: the retrying 503 was not observed on the board: {retrying:?}"
                        );
                    }
                }
                _ => unreachable!(),
            }
            eprintln!(
                "wave37-live: {program}/{scenario} ok; transcript={}",
                observed.run_dir.display()
            );
        }
    }
    let board = engine.board();
    for agent in &board.agents {
        eprintln!(
            "wave37-live: board {} program={} runs={} recent={:?}",
            agent.name, agent.program, agent.runs_total, agent.recent
        );
    }
    engine.shutdown(Duration::from_secs(10));
}
