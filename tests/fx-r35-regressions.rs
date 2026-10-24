//! fx-r35 regressions: durable endings before handle removal, refused reads warned,
//! truthful model summaries, and the per-request branch. The process tests spawn the
//! real `--run-agent` with a sleeping declared step and are serialized.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run;
use gewu_multiverse::board::Board;
use gewu_multiverse::board::status;
use gewu_multiverse::engine_api::{AgentView, BoardView, Command as BoardCommand, EngineApi};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-live-guard-{label}-{}-{}",
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

fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn group_members(pgid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(state) = fields.next().and_then(|state| state.chars().next()) else {
            continue;
        };
        let _ppid = fields.next();
        let Some(group) = fields.next().and_then(|group| group.parse::<u32>().ok()) else {
            continue;
        };
        if state != 'Z' && group == pgid {
            found.push(pid);
        }
    }
    found
}

fn write_process_records(run_dir: &Path, pid: u32) {
    mode::write_private(
        run_dir.join(gewu_multiverse::engine::session::PID_FILE),
        pid.to_string().as_bytes(),
    )
    .expect("session pid");
    mode::write_private(run_dir.join(run::PGID_FILE), pid.to_string().as_bytes())
        .expect("session pgid");
}

/// One real run directory whose declared step is a sleeping shell command.
fn write_sleeping_run(root: &Path, label: &str) -> PathBuf {
    let config = root.join("agents").join("agent-1");
    let skills = config.join("skills");
    let workspace = config.join("workspace");
    fs::create_dir_all(&skills).expect("skills");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(config.join("instruction.md"), "wait here\n").expect("instruction");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        b"[model_keys]\n\"agent-1\" = \"gw-synthetic-not-a-real-key\"\n",
    )
    .expect("credentials");
    let run_dir = root.join("state").join("agent-1").join(label);
    fs::create_dir_all(&run_dir).expect("run dir");
    RunPlan {
        run_id: label.to_owned(),
        agent: "agent-1".to_owned(),
        workspace,
        instruction: config.join("instruction.md"),
        skills_dir: skills,
        endpoint: "http://127.0.0.1:9/v1/chat/completions".to_owned(),
        model: "synthetic-model".to_owned(),
        credentials,
        max_turns: 2,
        steps: vec!["sleep 30".to_owned()],
        timeout_secs: 60,
        confinement: "none".to_owned(),
        hide: Vec::new(),
    }
    .write(&run_dir)
    .expect("the plan is written");
    run_dir
}

fn spawn_supervisor(run_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("the supervisor starts")
}

fn wait_for_shell(run_dir: &Path, pgid: u32) {
    assert!(
        wait_until(Duration::from_secs(10), || {
            run_dir.join(run::SHELL_PGID_FILE).is_file() && !group_members(pgid).is_empty()
        }),
        "the supervisor's shell never appeared in its group"
    );
}

fn stop_group(pgid: u32) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
}

fn wait_child(child: &mut Child) {
    let _ = wait_until(Duration::from_secs(5), || {
        child.try_wait().is_ok_and(|status| status.is_some())
    });
}

fn terminal(run_dir: &Path) -> Option<Record> {
    transcript::terminal_record(run_dir)
}

/// A normal stop must leave a durable ending before the identity handles go, so a
/// later open and Start cannot block on a finished run. The old order (reap, then
/// clear the handles without an ending) is kept as the explicit failing control.
#[test]
fn a_normal_stop_records_a_durable_ending_before_handles_and_reopens_clean() {
    let _process = process_test_lock();
    let root = scratch("normal-stop");
    let run_dir = write_sleeping_run(&root, "run-1700000100001-0001");
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    write_process_records(&run_dir, pgid);
    wait_for_shell(&run_dir, pgid);
    stop_group(pgid);
    wait_child(&mut supervisor);
    let settled = run::settle_run(
        &run_dir,
        pgid,
        run::RunOutcome::Stopped,
        Some("the run was stopped on request".to_owned()),
        Duration::from_secs(3),
    )
    .expect("the normal stop settles");
    let _ = settled;
    assert!(
        group_members(pgid).is_empty(),
        "the stopped group was still alive"
    );
    let record = terminal(&run_dir).expect("the normal stop left no ending");
    assert!(
        format!("{record:?}").contains("stopped"),
        "the ending is not stopped: {record:?}"
    );
    for name in [
        gewu_multiverse::engine::session::PID_FILE,
        run::PGID_FILE,
        run::SHELL_PID_FILE,
        run::SHELL_PGID_FILE,
        run::SHELL_IDENTITY_FILE,
        run::SHELL_CAPABILITY_FILE,
    ] {
        assert!(
            !run_dir.join(name).exists(),
            "the identity handle {name} survived a durable ending"
        );
    }
    assert_eq!(
        run::reap_leftover_run(&run_dir, Duration::from_millis(100)),
        Ok(false),
        "a cleanly stopped run was reported as uncertain"
    );

    // The old order on the same code path: reap, then clear without writing the
    // ending. That is the v0.1.34 record that blocked a clean agent forever.
    let old = write_sleeping_run(&root, "run-1700000100001-0002");
    let mut supervisor = spawn_supervisor(&old);
    let pgid = supervisor.id();
    write_process_records(&old, pgid);
    wait_for_shell(&old, pgid);
    stop_group(pgid);
    wait_child(&mut supervisor);
    let ended = wait_until(Duration::from_secs(3), || group_members(pgid).is_empty());
    assert!(
        ended,
        "the control's group did not end before the old boundary ran: {:?}",
        group_members(pgid)
    );
    let _ = run::reap_process_group(pgid, &old, Duration::from_secs(3));
    run::clear_process_records(&old);
    assert!(
        terminal(&old).is_none(),
        "the control unexpectedly wrote an ending"
    );
    let error = run::reap_leftover_run(&old, Duration::from_millis(100))
        .expect_err("an ending-less started run must stay uncertain");
    assert!(
        error.contains("cleanup unverified") && error.contains("cannot be verified"),
        "the uncertainty was not named honestly: {error}"
    );
    if std::env::var_os("GEWU_LIVE_KEEP").is_none() {
        let _ = fs::remove_dir_all(&root);
    } else {
        eprintln!("kept evidence root {}", root.display());
    }
}

/// A product killed with `SIGKILL` leaves the shell; the next open finds it by live
/// identity, stops it, writes the ending and only then clears the handles.
#[test]
fn a_killed_product_reopen_reaps_the_real_sleeping_child_and_records_its_ending() {
    let _process = process_test_lock();
    let root = scratch("kill-reopen");
    let run_dir = write_sleeping_run(&root, "run-1700000100002-0001");
    let mut supervisor = spawn_supervisor(&run_dir);
    let pgid = supervisor.id();
    write_process_records(&run_dir, pgid);
    wait_for_shell(&run_dir, pgid);
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pgid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    wait_child(&mut supervisor);
    assert!(
        !group_members(pgid).is_empty(),
        "the killed supervisor's shell did not survive; the reopen cannot be proven"
    );
    let reaped = run::reap_leftover_run(&run_dir, Duration::from_secs(3)).expect("the reopen reap");
    assert!(reaped, "the left-over group was not reported as reaped");
    assert!(
        group_members(pgid).is_empty(),
        "a run process survived the reopen"
    );
    assert!(
        run_dir.join(run::STOPPED_FILE).is_file(),
        "no run-stopped record"
    );
    assert!(
        format!("{:?}", terminal(&run_dir)).contains("stopped"),
        "no durable stopped ending"
    );
    assert!(
        !run_dir
            .join(gewu_multiverse::engine::session::PID_FILE)
            .exists(),
        "the identity handle survived the verified cleanup"
    );
    // A later run in the same installation is a new run id; no duplicate of the old
    // one exists and the old group stays empty while the new supervisor starts.
    let next = write_sleeping_run(&root, "run-1700000100002-0002");
    let mut next_supervisor = spawn_supervisor(&next);
    let next_pgid = next_supervisor.id();
    write_process_records(&next, next_pgid);
    wait_for_shell(&next, next_pgid);
    assert!(
        group_members(pgid).is_empty(),
        "the old group was repopulated beside the new run"
    );
    assert!(
        !group_members(next_pgid).is_empty(),
        "the replacement run did not start exactly one group"
    );
    stop_group(next_pgid);
    wait_child(&mut next_supervisor);
    let _ = run::settle_run(
        &next,
        next_pgid,
        run::RunOutcome::Stopped,
        Some("test cleanup".to_owned()),
        Duration::from_secs(3),
    );
    let _ = fs::remove_dir_all(&root);
}

/// An ending that cannot be written must keep the handles and block a replacement;
/// after the obstruction is repaired the same retry writes the ending and clears.
#[test]
fn an_interrupted_ending_write_keeps_the_handles_and_is_retried() {
    let _process = process_test_lock();
    let root = scratch("partial-write");
    let run_dir = root
        .join("state")
        .join("agent-1")
        .join("run-1700000100003-0001");
    fs::create_dir_all(&run_dir).expect("run dir");
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the owned child starts");
    let pid = child.id();
    run::write_shell_record(&run_dir, pid, pid).expect("the shell identity record");
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o555))
        .expect("the run directory becomes read-only");
    let error = run::reap_leftover_run(&run_dir, Duration::from_millis(300))
        .expect_err("an ending that cannot be written is a cleanup failure");
    assert!(
        error.contains("cannot be written"),
        "the ending failure was not named: {error}"
    );
    assert!(
        terminal(&run_dir).is_none(),
        "an ending was claimed while its record could not be written"
    );
    assert!(
        run_dir.join(run::SHELL_IDENTITY_FILE).is_file(),
        "the identity evidence was removed after a failed ending write"
    );
    assert!(
        group_members(pid).is_empty(),
        "the owned child survived the first cleanup pass"
    );
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
        .expect("the run directory is repaired");
    let retried = run::reap_leftover_run(&run_dir, Duration::from_millis(300))
        .expect("the repaired retry writes the ending");
    assert!(!retried, "the second pass reported a live group");
    assert!(
        terminal(&run_dir).is_some(),
        "the repaired retry wrote no ending"
    );
    assert!(
        !run_dir.join(run::SHELL_IDENTITY_FILE).exists(),
        "the repaired retry kept the stale identity"
    );
    let _ = child.wait();
    let _ = fs::remove_dir_all(&root);
}

fn prepare_scripted(root: &Path, instruction: &str) -> PathBuf {
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
    let root = scratch(label);
    let run_dir = prepare_scripted(&root, "probe the protected tree\n");
    let model = Stub::start(vec![
        StubReply::json(tool_call(command)),
        StubReply::json(final_reply(claim)),
    ]);
    let mut plan = RunPlan::read(&run_dir).expect("plan");
    plan.endpoint = model.url();
    plan.write(&run_dir).expect("plan");
    agent_run::execute(&run_dir);
    let (outcome, reason) = match terminal(&run_dir) {
        Some(Record::RunFinished {
            outcome, reason, ..
        }) => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    (run_dir, outcome, reason)
}

/// A read-only probe of an unreadable path is a warned run, never a refused write.
#[test]
fn a_refused_read_is_warned_and_never_wears_the_write_refusal_row() {
    let root = scratch("read-denied");
    let denied = root.join("locked");
    fs::create_dir_all(&denied).expect("the unreadable directory");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).expect("deny the reads");
    let path = denied.display().to_string();
    let command = format!(
        "if [ -r {path} ]; then echo READABLE; else echo READ-DENIED; fi\n\
         if ls {path} >/dev/null 2>&1; then echo LISTED; else echo LIST-DENIED; fi\n\
         if head -c 1 {path}/x >/dev/null 2>&1; then echo HEAD-OK; else echo HEAD-DENIED; fi\n\
         echo probe-done\n"
    );
    let (run_dir, outcome, reason) =
        run_scripted("refused-read", &command, "The read-only probe is finished.");
    assert_eq!(
        outcome, "warned",
        "a refused read became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the warned cause");
    assert!(
        !failure.starts_with("write refused:"),
        "a refused read wrote the write-refusal row: {failure}"
    );
    assert!(
        !failure.contains("DENIED"),
        "a later successful segment's stdout was passed off as the failed read's cause: {failure}"
    );
    // The same evidence on the Agent screen: the warned mark and the tool's own
    // row, never `⊘ write refused:`.
    let mut view = board_view(&run_dir, &outcome, &reason);
    view.agents[0].last_failure = Some(failure);
    let frame = render_agent(&mut view, 80, 24);
    assert!(frame.contains('⚠'), "the warned mark is missing:\n{frame}");
    assert!(
        !frame.contains("⊘ write refused:"),
        "a refused read still shows the write row:\n{frame}"
    );
    assert!(
        frame.contains("⚠ shell:"),
        "the warned command row is missing:\n{frame}"
    );
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).expect("restore");
    let _ = fs::remove_dir_all(&root);
}

/// The control: a real refused write is still a refused run with `⊘ write refused:`.
#[test]
fn a_real_refused_write_still_wears_the_write_mark_and_row() {
    let root = scratch("write-refused");
    let locked = root.join("locked-write");
    fs::create_dir_all(&locked).expect("the directory");
    fs::write(locked.join("keep.txt"), b"keep").expect("the file");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("read-only");
    let target = locked.join("blocked.txt").display().to_string();
    let (run_dir, outcome, reason) = run_scripted(
        "refused-write-control",
        &format!("rm {target} && touch {target}"),
        "I could not write the file.",
    );
    assert_eq!(
        outcome, "refused",
        "a real refused write was {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the refused cause");
    assert!(
        failure.starts_with("write refused:"),
        "the write refusal lost its row: {failure}"
    );
    let mut view = board_view(&run_dir, &outcome, &reason);
    view.agents[0].last_failure = Some(failure);
    let frame = render_agent(&mut view, 80, 24);
    assert!(
        frame.contains("⊘ write refused:"),
        "the write refusal row is missing:\n{frame}"
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restore");
    let _ = fs::remove_dir_all(&root);
}

fn board_view(_run_dir: &Path, outcome: &str, reason: &str) -> BoardView {
    let end = match outcome {
        "warned" => gewu_multiverse::engine_api::SessionEnd::Warned {
            reason: reason.to_owned(),
        },
        "refused" => gewu_multiverse::engine_api::SessionEnd::Refused {
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
            unknown_program: None,
            program_installed: true,
            program_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            protocol: "chat-completions".to_owned(),
            base_url: "https://model.example/v1".to_owned(),
            model: "demo".to_owned(),
            key_present: true,
            program_error: None,
            active_program: None,
            next_program: None,
            codex_sandbox: None,
        }],
        programs: Vec::new(),
        endpoint: gewu_multiverse::engine_api::EndpointView {
            configured: true,
            url: "https://model.example/v1/chat/completions".to_owned(),
            base_url: "https://model.example/v1".to_owned(),
            protocol: "chat-completions".to_owned(),
            model: "demo".to_owned(),
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
    fn shutdown(&mut self, _grace: Duration) {}
}

fn draw(board: &mut Board, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a terminal");
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

/// The model's own final summary is not rewritten; ordinary words that happen to
/// match a status word are not replaced. The visible row may still be cut with an
/// explicit ellipsis, and the transcript keeps the whole sentence.
#[test]
fn model_summary_words_are_not_rewritten_while_status_labels_stay_exact() {
    let root = scratch("verbatim-summary");
    let run_dir = prepare_scripted(&root, "read the missing file\n");
    let mut view = board_view(&run_dir, "warned", "The endpoint session token is missing.");
    view.agents[0].last_result =
        Some("I could not read the missing file in the endpoint session.".to_owned());
    let frame = render_agent(&mut view, 120, 40);
    assert!(
        frame.contains("result: I could not read the missing file in the endpoint session"),
        "the model's words were rewritten:\n{frame}"
    );
    assert!(
        !frame.contains("not set up file"),
        "a status label was substituted into the model's sentence:\n{frame}"
    );
    // The engine's own exact status labels still map.
    assert_eq!(
        status::blocked_words("needs an instruction"),
        "needs an instruction"
    );
    assert_eq!(
        status::blocked_words("cleanup unverified: x"),
        "cleanup unverified"
    );
    assert_eq!(
        status::blocked_words("live leftover: pid 4"),
        "live leftover"
    );
    assert_eq!(
        status::blocked_words("unverified leftover: group 4"),
        "unverified leftover"
    );
    assert_eq!(status::blocked_words("something else"), "needs help");
    // The exact word `missing` alone is not a status label the product invents;
    // the Forgejo state has its own exact words.
    let mut forgejo = board_view(&run_dir, "warned", "x");
    forgejo.forgejo.state = gewu_multiverse::engine_api::ForgejoState::Missing;
    let mut board = Board::new(Fixed(forgejo));
    let frame = text(&draw(&mut board, 120, 40));
    assert!(
        frame.contains("not set up yet"),
        "the missing state lost its word"
    );
    let _ = fs::remove_dir_all(&root);
}

/// A successful move followed by a failed read of the moved-away name is a read
/// warning, not a refused write, even though the failed name was a move operand.
#[test]
fn a_successful_move_then_failed_read_of_that_name_is_a_warning() {
    let (run_dir, outcome, reason) = run_scripted(
        "read-write-collision",
        "printf x > f.txt; mv f.txt g.txt; cat f.txt; echo done\n",
        "The inspection is finished.",
    );
    assert_eq!(
        outcome, "warned",
        "a failed read after a successful move became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the warned cause");
    assert!(
        !failure.starts_with("write refused:"),
        "the successful move's target turned the failed read into a write refusal: {failure}"
    );
    assert!(
        failure.contains("cat:") && failure.contains("f.txt"),
        "the read's own cause was lost: {failure}"
    );
    let mut view = board_view(&run_dir, &outcome, &reason);
    view.agents[0].last_failure = Some(failure);
    let frame = render_agent(&mut view, 80, 24);
    assert!(frame.contains('⚠'), "the warned mark is missing:\n{frame}");
    assert!(
        !frame.contains("⊘ write refused:"),
        "the collision still wears the write mark:\n{frame}"
    );
    let _ = fs::remove_dir_all(&run_dir);
}

/// The control: a hidden write failure behind a zero wrapper is still refused, and
/// a failed write's own diagnostic still wins over a later read line.
#[test]
fn a_hidden_write_failure_behind_a_zero_wrapper_is_still_refused() {
    let (run_dir, outcome, reason) = run_scripted(
        "hidden-write-control",
        "mkdir -p ro && chmod 555 ro; if cat does-not-exist; then :; fi; printf x > ro/file; echo done\n",
        "The write is finished.",
    );
    assert_eq!(
        outcome, "refused",
        "a hidden write failure became {outcome}: {reason}"
    );
    let failure = run::read_failure(&run_dir).expect("the refused cause");
    assert!(
        failure.starts_with("write refused:"),
        "the hidden write lost its refusal row: {failure}"
    );
    let mut view = board_view(&run_dir, &outcome, &reason);
    view.agents[0].last_failure = Some(failure);
    let frame = render_agent(&mut view, 80, 24);
    assert!(
        frame.contains("⊘ write refused:"),
        "the hidden write row is missing:\n{frame}"
    );
    let _ = fs::remove_dir_all(&run_dir);
}

/// A supervisor outcome is not cleanup proof while a capability record exists. This
/// environment hides non-dumpable processes from `/proc`, so the live-holder shape is
/// The live-holder recovery boundary; its invariant is checked here with the capability record
/// and no verified holder: the ending must not clear the evidence or write a marker.
#[test]
fn a_terminal_outcome_does_not_clear_unverified_capability_evidence() {
    let _process = process_test_lock();
    let root = scratch("terminal-capability-uncertain");
    let run_dir = root
        .join("state")
        .join("agent-1")
        .join("run-1700000100009-0001");
    fs::create_dir_all(&run_dir).expect("run dir");
    let capability = run::prepare_shell_capability(&run_dir).expect("capability record");
    drop(capability);
    let mut transcript = transcript::Transcript::open(&run_dir).expect("transcript");
    transcript
        .append(&Record::run_started(
            "run-1700000100009-0001",
            "agent-1",
            424_242,
            "/tmp",
        ))
        .expect("typed started");
    transcript::record_outcome_if_missing(
        &run_dir,
        "run-1700000100009-0001",
        run::RunOutcome::Succeeded,
        Some("the supervisor ended".to_owned()),
        0,
    )
    .expect("typed ending");
    // An unrelated live process must stay untouched while the capability evidence is
    // unresolved.
    let mut unrelated = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("unrelated child");
    let unrelated_pid = unrelated.id();
    let result = run::reap_leftover_run(&run_dir, Duration::from_millis(150));
    let unrelated_alive = unrelated.try_wait().expect("status").is_none();
    let capability_retained = run_dir.join(run::SHELL_CAPABILITY_FILE).is_file();
    let false_marker = run_dir.join(run::STOPPED_FILE).is_file();
    if unrelated_alive {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(unrelated_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = unrelated.wait();
    assert!(unrelated_alive, "an unrelated process was signalled");
    assert!(
        capability_retained,
        "a terminal outcome cleared the capability evidence with no verified holder"
    );
    assert!(
        !false_marker,
        "a terminal outcome wrote a false cleanup marker"
    );
    let error = result.expect_err("no verified holder is unresolved uncertainty");
    assert!(
        error.starts_with("cleanup unverified:"),
        "the uncertainty was not categorized honestly: {error}"
    );
    assert!(
        error.contains("capability") && error.contains("no verified holder"),
        "the capability uncertainty lost its reason: {error}"
    );
    let _ = fs::remove_dir_all(&root);
}
