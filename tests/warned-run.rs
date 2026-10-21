//! Correction 4's new mark: a clean run whose tool results include a failed command
//! is `⚠`, never `✓`, and the failed command's cause is its row.
//!
//! The run is real (`agent_run::execute`) against a scripted loopback model; the
//! board state is built from the same terminal record and `last-failure` file the
//! engine reads, so the frame is the one a person would see. This file names the new
//! `SessionEnd::Warned` vocabulary, so it is the render-level half of the guard; the
//! behaviour-level half (the terminal word) is in `refused-write.rs` and compiles on
//! the reviewed head.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run;
use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi, SessionEnd, SessionState};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-warned-guard-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the scratch root");
    root
}

fn prepare(root: &Path, instruction: &str) -> PathBuf {
    let config = root.join("agents").join("scribe");
    let skills = config.join("skills");
    let workspace = config.join("workspace");
    fs::create_dir_all(&skills).expect("skills");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(config.join("instruction.md"), instruction).expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("scribe").join("run-1");
    fs::create_dir_all(&run_dir).expect("run directory");
    RunPlan {
        run_id: "run-1".to_owned(),
        agent: "scribe".to_owned(),
        workspace,
        instruction: config.join("instruction.md"),
        skills_dir: skills,
        endpoint: String::new(),
        model: "example-model".to_owned(),
        credentials,
        max_turns: 4,
        steps: Vec::new(),
        timeout_secs: 30,
        confinement: "none".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan is written");
    run_dir
}

fn tool_call(command: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_write", "type": "function",
                    "function": {"name": "shell",
                        "arguments": serde_json::json!({"command": command}).to_string()}}]}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}
    })
    .to_string()
}

fn final_reply(text: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}
    })
    .to_string()
}

fn git(args: &[&str], dir: &Path) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
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

fn init_repo_with_commit(workspace: &Path) {
    git(&["init", "-q", "--initial-branch=main"], workspace);
    fs::write(workspace.join("README.md"), "hello\n").expect("a README");
    git(&["add", "README.md"], workspace);
    git(
        &[
            "-c",
            "user.name=gewu test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            "init",
        ],
        workspace,
    );
}

fn terminal(run_dir: &Path) -> (String, String) {
    transcript::terminal_record(run_dir)
        .and_then(|record| match record {
            Record::RunFinished {
                outcome, reason, ..
            } => Some((outcome, reason.unwrap_or_default())),
            _ => None,
        })
        .expect("the terminal record")
}

fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            out.push_str(buffer.cell((x, y)).expect("a cell").symbol());
        }
        out.push('\n');
    }
    out
}

/// An engine whose newest run is the warned one, with the same two rows the engine
/// puts on the card: the failed command's cause and the model's own result.
struct Warned {
    inner: FakeEngine,
    reason: String,
    failure: String,
}

impl EngineApi for Warned {
    fn board(&mut self) -> BoardView {
        let mut view = self.inner.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![SessionEnd::Warned {
                reason: self.reason.clone(),
            }];
            agent.last_result = Some(self.reason.clone());
            agent.last_failure = Some(self.failure.clone());
            agent.last_error = None;
            agent.session = SessionState::Idle;
        }
        view
    }
    fn send(&mut self, command: Command) -> Result<(), String> {
        self.inner.send(command)
    }
    fn tick(&mut self) {
        self.inner.tick()
    }
    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.inner.instruction_path(agent)
    }
    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.inner.session_files(agent)
    }
    fn shutdown(&mut self, grace: std::time::Duration) {
        self.inner.shutdown(grace)
    }
}

fn ready_engine() -> FakeEngine {
    let mut engine = FakeEngine::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetEndpoint {
            url: "https://model.example/v1/chat/completions".to_owned(),
            model: "demo".to_owned(),
            key: Some("demo-key".to_owned()),
        })
        .expect("endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("an agent");
    engine.send(Command::StartForgejo).expect("Forgejo");
    for _ in 0..160 {
        engine.tick();
    }
    engine
}

fn frame(board: &mut Board, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    text(terminal.backend().buffer())
}

#[test]
fn a_failed_command_is_a_warning_with_its_cause_and_never_a_tick() {
    let root = scratch_root("show-push");
    let run_dir = prepare(&root, "Review the work.\n");
    let workspace = root.join("agents").join("scribe").join("workspace");
    init_repo_with_commit(&workspace);
    let claim = "The review is complete.";
    let model = Stub::start(vec![
        StubReply::json(tool_call("git show push")),
        StubReply::json(final_reply(claim)),
    ]);
    let mut plan = RunPlan::read(&run_dir).expect("the plan reads");
    plan.endpoint = model.url();
    plan.write(&run_dir).expect("the plan is rewritten");
    let end = agent_run::execute(&run_dir);
    let (outcome, reason) = terminal(&run_dir);
    let failure = run::read_failure(&run_dir).expect("the failed command was recorded");
    assert!(end.exit_code == 0, "the agent process still exits 0");
    assert_eq!(outcome, "warned", "a failed read was not a warning");
    assert_eq!(reason, claim, "the model's sentence is the result row");
    assert!(
        failure.contains("ambiguous argument 'push'"),
        "the failure row lost the cause: {failure}"
    );

    let mut board = Board::new(Warned {
        inner: ready_engine(),
        reason: reason.clone(),
        failure: failure.clone(),
    });
    board.on_key(KeyEvent::from(KeyCode::Enter));
    for (width, height) in [(72, 18), (80, 24), (120, 40)] {
        let frame = frame(&mut board, width, height);
        assert!(
            frame.contains("last runs ⚠"),
            "{width}x{height}: no warned mark:\n{frame}"
        );
        assert!(
            !frame.contains("last runs ✓"),
            "{width}x{height}: the run still wears a green tick:\n{frame}"
        );
        assert!(
            frame.contains("⚠ shell: exit 128"),
            "{width}x{height}: the failed command's reason is not the row:\n{frame}"
        );
        assert!(
            !frame.contains("✗ shell: exit"),
            "{width}x{height}: a failed command still wears ✗:\n{frame}"
        );
        assert!(
            frame.contains("result: The review is complete"),
            "{width}x{height}: the model's result row is missing:\n{frame}"
        );
        assert!(
            frame.contains("⚠ cmd"),
            "{width}x{height}: the legend has no warning entry:\n{frame}"
        );
        assert!(
            frame.contains("⊘ refused") && frame.contains("◷ timed out"),
            "{width}x{height}: the earlier legend entries were cut:\n{frame}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
