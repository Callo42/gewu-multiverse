//! Regressions for the honesty and guidance classes.
//!
//! The run tests execute the real `agent_run::execute` with a scripted loopback model
//! and a real shell: a compound read-only probe with `/dev/null` redirections, and a
//! denied read whose long path used to consume the whole cause. The board assertions
//! use the same run record the product writes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run;
use gewu_multiverse::board::Board;
use gewu_multiverse::engine_api::{AgentView, BoardView, Command as BoardCommand, EngineApi};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-regression-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
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
    .expect("the plan");
    run_dir
}

fn tool_call(command: &str) -> String {
    serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
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

fn run_scripted(label: &str, command: &str, claim: &str) -> (PathBuf, String, String) {
    run_scripted_with(label, |_| {}, command, claim)
}

fn run_scripted_with(
    label: &str,
    setup: impl FnOnce(&Path),
    command: &str,
    claim: &str,
) -> (PathBuf, String, String) {
    let root = scratch(label);
    let run_dir = prepare(&root, "probe the protected tree\n");
    let workspace = root.join("agents").join("scribe").join("workspace");
    setup(&workspace);
    let model = Stub::start(vec![
        StubReply::json(tool_call(command)),
        StubReply::json(final_reply(claim)),
    ]);
    let mut plan = RunPlan::read(&run_dir).expect("plan");
    plan.endpoint = model.url();
    plan.write(&run_dir).expect("plan");
    agent_run::execute(&run_dir);
    let (outcome, reason) = match transcript::terminal_record(&run_dir) {
        Some(Record::RunFinished {
            outcome, reason, ..
        }) => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    (run_dir, outcome, reason)
}

fn board_view(_run_dir: &Path, outcome: &str, reason: &str) -> BoardView {
    let end = match outcome {
        "warned" => gewu_multiverse::engine_api::SessionEnd::Warned {
            reason: reason.to_owned(),
        },
        _ => gewu_multiverse::engine_api::SessionEnd::Succeeded,
    };
    BoardView {
        terms_acknowledged: true,
        terms: Vec::new(),
        setup_complete: true,
        forgejo: gewu_multiverse::engine_api::ForgejoView {
            state: gewu_multiverse::engine_api::ForgejoState::Running,
            url: "http://127.0.0.1:3000".to_owned(),
            version: "17.0.0".to_owned(),
            admin_login: "owner".to_owned(),
            organisation: "team".to_owned(),
            repositories: 1,
            repository_names: vec!["team/workspace".to_owned()],
            events: Vec::new(),
            admin_login_kept: false,
        },
        agents: vec![AgentView {
            name: "scribe".to_owned(),
            login: "gewu-scribe".to_owned(),
            provisioning: gewu_multiverse::engine_api::Provisioning::Ready,
            desired: gewu_multiverse::engine_api::Desired::Stopped,
            session: gewu_multiverse::engine_api::SessionState::Idle,
            sessions_since_start: 0,
            runs_total: 1,
            recent: vec![end],
            instruction_chars: 12,
            instruction_first_line: Some("probe the tree".to_owned()),
            instruction: Some("probe the tree\n".to_owned()),
            instruction_lines: 1,
            activity: None,
            activity_at: None,
            last_error: None,
            last_result: Some(reason.to_owned()),
            last_failure: None,
            leftover: None,
            current_tool: None,
            current_tool_seconds: 0,
            last_work: Vec::new(),
            no_assigned_work: false,
            last_work_result: None,
            last_result_is_work: false,
            program: "builtin".to_owned(),
            program_label: "built-in engine".to_owned(),
            program_installed: true,
            program_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            protocol: "chat-completions".to_owned(),
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            model: "example-model".to_owned(),
            key_present: true,
            program_error: None,
            active_program: None,
            next_program: None,
            codex_sandbox: None,
        }],
        programs: Vec::new(),
        endpoint: gewu_multiverse::engine_api::EndpointView {
            configured: true,
            url: "http://127.0.0.1:1/v1/chat/completions".to_owned(),
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "example-model".to_owned(),
            key_present: true,
            test: gewu_multiverse::engine_api::EndpointTest::NotRun,
        },
        notice: None,
        admin_password: None,
        confined: true,
        confinement: Some("Landlock".to_owned()),
        upgrade: None,
    }
}

struct Fixed(BoardView);

impl EngineApi for Fixed {
    fn board(&mut self) -> BoardView {
        self.0.clone()
    }
    fn send(&mut self, _command: BoardCommand) -> Result<(), String> {
        Ok(())
    }
    fn tick(&mut self) {}
    fn instruction_path(&self, _agent: &str) -> Option<PathBuf> {
        None
    }
    fn session_files(&self, _agent: &str) -> Option<(PathBuf, PathBuf)> {
        None
    }
    fn shutdown(&mut self, _grace: std::time::Duration) {}
}

fn draw(board: &mut Board, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    terminal.backend().buffer().clone()
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

fn render_agent(view: &mut BoardView, width: u16, height: u16) -> String {
    let mut board = Board::new(Fixed(view.clone()));
    board.on_key(KeyEvent::from(KeyCode::Enter));
    text(&draw(&mut board, width, height))
}

/// A real compound read-only probe shape: `/dev/null` on the read tests, `[ -d ]`,
/// `find`, `env`, and a `sed` filter. The only failures are reads. The run is `⚠`,
/// the card's row is the shell's own, and the product never borrows the write refusal.
#[test]
fn a_compound_read_only_probe_is_warned_without_an_internal_cause() {
    let root = scratch("compound-read");
    let denied = root.join("locked");
    let missing = root.join("missing-file");
    fs::create_dir_all(&denied).expect("the unreadable directory");
    fs::write(denied.join("keep"), b"keep").expect("a file");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).expect("deny the reads");
    let command = format!(
        "for p in {denied} {missing} /etc/hostname; do \
         if [ -f \"$p\" ] && cat \"$p\" >/dev/null 2>&1; then echo \"READABLE file $p\"; \
         elif [ -d \"$p\" ] && find \"$p\" -maxdepth 1 >/dev/null 2>&1; then echo \"READABLE dir $p\"; \
         else echo \"DENIED $p\"; fi; done; \
         echo \"--- environment ---\"; env | sort | sed -E 's/(KEY|TOKEN|SECRET|PASSWORD)=.*/\\\\1=<redacted-by-fixture>/I'",
        denied = denied.display(),
        missing = missing.display()
    );
    let (run_dir, outcome, reason) = run_scripted(
        "compound-read",
        &command,
        "Some reads were denied; the environment is in the tool output.",
    );
    assert_eq!(
        outcome, "warned",
        "the compound read-only probe became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the warned cause");
    assert!(
        failure.starts_with("shell:"),
        "the probe did not keep the shell's own row: {failure}"
    );
    assert!(
        !failure.starts_with("write refused:"),
        "the read-only probe wrote the write-refusal row: {failure}"
    );
    assert!(
        !failure.contains("GEWU_CONFINE_COMMAND") && !failure.contains("DENIED"),
        "the probe's stdout or wrapper became its cause: {failure}"
    );
    assert!(
        failure.contains("condition returned non-zero") || failure.contains("(find)"),
        "the failed read did not name its own status: {failure}"
    );
    let log = fs::read_to_string(run_dir.join(run::LOG_FILE)).expect("the run log");
    assert!(
        log.contains(&format!("⚠ {failure}")),
        "the card and run log disagree: card={failure:?}\n{log}"
    );
    assert!(
        !log.contains("write refused:"),
        "the run log still called the probe a refused write:\n{log}"
    );
    for (width, height) in [(120, 40), (72, 18)] {
        let mut view = board_view(&run_dir, &outcome, &reason);
        view.agents[0].last_failure = Some(failure.clone());
        let frame = render_agent(&mut view, width, height);
        assert!(
            frame.contains('⚠'),
            "no warning mark at {width}x{height}:\n{frame}"
        );
        assert!(
            !frame.contains("⊘ write refused:"),
            "the read-only probe wore the write row at {width}x{height}:\n{frame}"
        );
        assert!(
            !frame.contains("GEWU_CONFINE_COMMAND"),
            "the internal wrapper reached the screen at {width}x{height}:\n{frame}"
        );
    }
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).expect("restore");
    let _ = fs::remove_dir_all(&root);
}

/// A long denied path must not consume the useful `Permission denied` clause. At 80
/// columns the card keeps the program and the error tail, with an explicit cut.
#[test]
fn a_long_denied_read_keeps_its_error_clause_at_80_columns() {
    let root = scratch("long-denied-read");
    let long = root.join("a-rather-long-directory-name-that-keeps-going-and-going");
    fs::create_dir_all(&long).expect("the long directory");
    let file = long.join("credentials.toml");
    fs::write(&file, b"not a real secret").expect("the file");
    fs::set_permissions(&long, fs::Permissions::from_mode(0o000)).expect("deny the read");
    let command = format!("cat {}", file.display());
    let (run_dir, outcome, reason) = run_scripted(
        "long-denied-read",
        &command,
        "I could not read the credentials file: permission was denied.",
    );
    assert_eq!(
        outcome, "warned",
        "a denied read became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the warned cause");
    assert!(
        failure.contains("cat:") && failure.contains("Permission denied"),
        "the useful error clause was lost: {failure}"
    );
    assert!(
        failure.contains("…"),
        "the long path was not shortened explicitly: {failure}"
    );
    let mut view = board_view(&run_dir, &outcome, &reason);
    view.agents[0].last_failure = Some(failure);
    let frame = render_agent(&mut view, 80, 24);
    assert!(frame.contains('⚠'), "the warning mark is missing:\n{frame}");
    assert!(
        frame.contains("Permission denied") && frame.contains("cat:"),
        "the 80-column card lost the complete cause:\n{frame}"
    );
    assert!(
        !frame.contains("cat:…"),
        "the 80-column card reduced the cause to a bare program:\n{frame}"
    );
    fs::set_permissions(&long, fs::Permissions::from_mode(0o700)).expect("restore");
    let _ = fs::remove_dir_all(&root);
}

/// A medium denied-read path must keep its error clause at the two narrow supported
/// sizes. The cause is deliberately below the old 70-character shortcut threshold,
/// but wider than the card's one-row text area.
#[test]
fn a_medium_denied_read_keeps_permission_denied_at_72_and_80() {
    let dirname = "medium-denied-cause-12345";
    let relative = format!("{dirname}/credentials.toml");
    let (run_dir, outcome, reason) = run_scripted_with(
        "medium-denied-read",
        |workspace| {
            let dir = workspace.join(dirname);
            fs::create_dir_all(&dir).expect("the medium directory");
            fs::write(dir.join("credentials.toml"), b"not a real secret").expect("the file");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("deny the read");
        },
        &format!("cat {relative}"),
        "I could not read the credentials file: permission was denied.",
    );
    assert_eq!(
        outcome, "warned",
        "a medium denied read became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the warned cause");
    let cause = failure.strip_prefix("shell: exit 1: ").unwrap_or(&failure);
    assert!(
        cause.chars().count() > 55 && cause.chars().count() < 70,
        "the fixture did not make a medium cause ({} chars): {cause}",
        cause.chars().count()
    );
    assert!(
        failure.contains("cat:") && failure.contains("Permission denied"),
        "the useful error clause was lost from the record: {failure}"
    );
    for (width, height) in [(72, 18), (80, 24)] {
        let mut view = board_view(&run_dir, &outcome, &reason);
        view.agents[0].last_failure = Some(failure.clone());
        let frame = render_agent(&mut view, width, height);
        assert!(
            frame.contains("Permission denied") && frame.contains("cat:"),
            "{width}x{height}: the medium cause lost its error words:\n{frame}"
        );
        assert!(
            !frame.contains("cat:…"),
            "{width}x{height}: the medium cause still collapsed to a bare program:\n{frame}"
        );
    }
    let workspace = RunPlan::read(&run_dir)
        .expect("the plan")
        .workspace
        .join(dirname);
    let _ = fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700));
    let _ = fs::remove_dir_all(
        run_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .unwrap_or(&workspace),
    );
}

/// The baked upgrade line stays inside the evidence boundary: observed marks and
/// checked cleanup of an ordinary earlier run, not perfect history or unconditional
/// containment.
#[test]
fn the_upgrade_line_does_not_claim_perfect_marks_or_unconditional_containment() {
    let line = gewu_multiverse::engine::WHATS_NEW;
    assert!(!line.contains("never outlive"), "{line}");
    assert!(!line.contains("exactly what happened"), "{line}");
    assert!(line.contains("what was observed"), "{line}");
    assert!(line.contains("ordinary earlier run"), "{line}");
    assert!(line.contains("cleanup checked"), "{line}");
}
