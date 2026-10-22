//! Review-correction 2's B2: a refused destructive write must not look like `✓`.
//!
//! Each test runs a real agent run (`agent_run::execute`) with a scripted loopback
//! model. The model asks the shell for one `curl` write; the target answers 4xx and
//! `curl` exits 0; the model then claims success. The run's own tool result is the
//! witness, so the terminal record must say `refused`, and the Agent screen must draw
//! `⊘` with the run's own summary beside it.
//!
//! This file is also the guard for correction 2's non-blocking 4: a successful `mv`
//! whose output is only the word `refused` (a filename) stays `✓`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnd, RunEnding};
use gewu_multiverse::board::{Board, FakeEngine};
use gewu_multiverse::engine_api::{BoardView, Command, EngineApi, SessionEnd, SessionState};
use gewu_multiverse::mode;
use gewu_multiverse::run::RunPlan;
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-write-guard-{label}-{}-{}",
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

/// A real run directory and plan for one scripted agent.
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
        endpoint: String::new(), // replaced by the caller below
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

fn endpoint(run_dir: &Path, url: &str) {
    let mut plan = RunPlan::read(run_dir).expect("the plan reads");
    plan.endpoint = url.to_owned();
    plan.write(run_dir).expect("the plan is rewritten");
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

/// Run one agent whose model asks for `command`, with a target stub answering
/// `status` and `body`, and whose final sentence is `claim`.
fn run_command(
    label: &str,
    command_for: impl Fn(&str) -> String,
    status: u16,
    body: &str,
    claim: &str,
) -> (RunEnd, String, String) {
    run_scripted(
        label,
        "Delete the repository team/workspace.\n",
        |_, _| {},
        command_for,
        status,
        body,
        claim,
    )
}

/// Like [`run_command`], with a workspace setup hook that receives the loopback
/// base URL (for a git remote) and a chosen instruction.
fn run_scripted(
    label: &str,
    instruction: &str,
    setup: impl FnOnce(&Path, &str),
    command_for: impl Fn(&str) -> String,
    status: u16,
    body: &str,
    claim: &str,
) -> (RunEnd, String, String) {
    let (end, outcome, reason, _failure, _segments) =
        run_scripted_failure(label, instruction, setup, command_for, status, body, claim);
    (end, outcome, reason)
}

/// Like [`run_scripted`], also returning the `last-failure` line the card draws.
fn run_scripted_failure(
    label: &str,
    instruction: &str,
    setup: impl FnOnce(&Path, &str),
    command_for: impl Fn(&str) -> String,
    status: u16,
    body: &str,
    claim: &str,
) -> (
    RunEnd,
    String,
    String,
    Option<String>,
    Vec<gewu_multiverse::transcript::CommandSegment>,
) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, instruction);
    let workspace = root.join("agents").join("scribe").join("workspace");
    let target = Stub::start(vec![StubReply::status(status, body)]);
    let base = format!("http://{}", target.addr());
    setup(&workspace, &base);
    let command = command_for(&base);
    let model = Stub::start(vec![
        StubReply::json(tool_call(&command)),
        StubReply::json(final_reply(claim)),
    ]);
    endpoint(&run_dir, &model.url());
    let end = agent_run::execute(&run_dir);
    let (outcome, reason) = terminal(&run_dir);
    let failure = gewu_multiverse::run::read_failure(&run_dir);
    let segments = transcript::read(&run_dir)
        .ok()
        .and_then(|file| {
            file.records.into_iter().find_map(|record| match record {
                Record::ToolResult { tool, segments, .. } if tool == "shell" => Some(segments),
                _ => None,
            })
        })
        .unwrap_or_default();
    let _ = fs::remove_dir_all(&root);
    (end, outcome, reason, failure, segments)
}

/// One run with no tool call: the model's final sentence is the only evidence.
fn run_reply(label: &str, instruction: &str, claim: &str) -> (RunEnd, String, String) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, instruction);
    let model = Stub::start(vec![StubReply::json(final_reply(claim))]);
    endpoint(&run_dir, &model.url());
    let end = agent_run::execute(&run_dir);
    let (outcome, reason) = terminal(&run_dir);
    let _ = fs::remove_dir_all(&root);
    (end, outcome, reason)
}

/// Run git in a scratch directory with no host configuration to inherit.
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

/// A workspace repository with one commit on `main`.
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

/// An engine whose newest run has a chosen mark, result and failure row.
struct Recorded {
    inner: FakeEngine,
    end: SessionEnd,
    reason: String,
    failure: Option<String>,
}

impl EngineApi for Recorded {
    fn board(&mut self) -> BoardView {
        let mut view = self.inner.board();
        if let Some(agent) = view.agents.first_mut() {
            agent.recent = vec![self.end.clone()];
            agent.last_result = Some(self.reason.clone());
            agent.last_error = None;
            agent.last_failure = self.failure.clone();
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

fn card(reason: &str) -> String {
    card_end(
        SessionEnd::Refused {
            reason: reason.to_owned(),
        },
        reason,
        None,
    )
}

fn card_end(end: SessionEnd, reason: &str, failure: Option<String>) -> String {
    let mut board = Board::new(Recorded {
        inner: ready_engine(),
        end,
        reason: reason.to_owned(),
        failure,
    });
    board.on_key(KeyEvent::from(KeyCode::Enter));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("a terminal");
    terminal
        .draw(|frame| board.draw(frame))
        .expect("the frame draws");
    text(terminal.backend().buffer())
}

#[test]
fn xdelete_with_a_403_is_not_a_green_tick() {
    // Review correction 2, run 0008: `-XDELETE` was not recognised as a write.
    let (end, outcome, reason) = run_command(
        "xdelete-403",
        |url| format!("curl -s -XDELETE {url}"),
        403,
        "Forbidden",
        "I deleted team/workspace successfully.",
    );
    assert!(end.is_success(), "curl exits 0 on a 403");
    assert_eq!(end.ending, RunEnding::Refused, "{end:?}");
    assert_eq!(outcome, "refused", "the card would say success");
    assert!(
        reason.contains("I deleted team/workspace successfully"),
        "{reason}"
    );
    let frame = card(&reason);
    assert!(frame.contains("⊘"), "the refused mark is missing:\n{frame}");
    assert!(
        frame.contains("result: I deleted team/workspace successfully"),
        "the result row is missing:\n{frame}"
    );
}

#[test]
fn canonical_delete_with_a_401_is_not_a_green_tick() {
    // Review correction 2, run 0009: `-X DELETE` was recognised but `Unauthorized`
    // was not, and the product itself calls a 401 "refused".
    let (end, outcome, reason) = run_command(
        "delete-401",
        |url| format!("curl -s -X DELETE {url}"),
        401,
        "Unauthorized",
        "I deleted team/workspace successfully.",
    );
    assert!(end.is_success());
    assert_eq!(end.ending, RunEnding::Refused, "{end:?}");
    assert_eq!(outcome, "refused");
    let frame = card(&reason);
    assert!(frame.contains("⊘"), "{frame}");
    assert!(
        frame.contains("result: I deleted team/workspace successfully"),
        "{frame}"
    );
}

#[test]
fn a_successful_write_listing_a_file_named_refused_is_still_a_tick() {
    // Review correction 2, non-blocking 4: only the bare word `refused` was in the
    // output. It is a filename, not a refusal.
    let root = scratch_root("refused-word");
    let run_dir = prepare(&root, "Rename the README and report.\n");
    fs::write(
        root.join("agents")
            .join("scribe")
            .join("workspace")
            .join("README.md"),
        "hello\n",
    )
    .expect("a README");
    let model = Stub::start(vec![
        StubReply::json(tool_call("mv README.md refused && ls")),
        StubReply::json(final_reply(
            "The README is renamed and the work is complete.",
        )),
    ]);
    endpoint(&run_dir, &model.url());
    let end = agent_run::execute(&run_dir);
    let (outcome, _reason) = terminal(&run_dir);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(end.ending, RunEnding::Succeeded, "{end:?}");
    assert_eq!(outcome, "succeeded", "a filename is not a refusal");
}

#[test]
fn a_git_push_with_a_global_option_is_not_a_green_tick() {
    // Review correction 3, blocking 1: `git -C . push` was not seen as a write at
    // all, so the 403 was a green `✓` beside "The work is pushed and complete."
    let (end, outcome, reason) = run_scripted(
        "git-c-403",
        "Push the work to the server.\n",
        |workspace, base| {
            init_repo_with_commit(workspace);
            git(
                &["remote", "add", "origin", &format!("{base}/team/repo.git")],
                workspace,
            );
        },
        |_| "git -C . push origin main".to_owned(),
        403,
        "Forbidden",
        "The work is pushed and complete.",
    );
    assert!(end.is_success(), "the agent process still exits 0");
    assert_eq!(end.ending, RunEnding::Refused, "{end:?}");
    assert_eq!(outcome, "refused", "a refused push wore the success word");
    assert!(
        reason.contains("The work is pushed and complete"),
        "{reason}"
    );
    let frame = card(&reason);
    assert!(frame.contains("⊘"), "the refused mark is missing:\n{frame}");
    assert!(
        frame.contains("result: The work is pushed and complete"),
        "the result row is missing:\n{frame}"
    );
}

#[test]
fn a_pre_receive_rejection_is_not_a_green_tick() {
    // Review correction 3, blocking 2: the server rejects the push; git exits 1.
    // The write failed, so the run is a refusal whatever its output says.
    let (end, outcome, reason) = run_scripted(
        "git-hook",
        "Push the work to the server.\n",
        |workspace, _base| {
            let bare = workspace.join("origin.git");
            git(
                &["init", "--bare", "-q", bare.to_str().expect("a UTF-8 path")],
                workspace,
            );
            let hooks = bare.join("hooks");
            fs::create_dir_all(&hooks).expect("the hooks directory");
            let hook = hooks.join("pre-receive");
            fs::write(
                &hook,
                "#!/bin/sh\necho 'blocked by server policy' >&2\nexit 1\n",
            )
            .expect("the hook");
            use std::os::unix::fs::PermissionsExt;
            let mut mode = fs::metadata(&hook).expect("the hook's mode").permissions();
            mode.set_mode(0o755);
            fs::set_permissions(&hook, mode).expect("an executable hook");
            init_repo_with_commit(workspace);
            git(
                &[
                    "remote",
                    "add",
                    "origin",
                    bare.to_str().expect("a UTF-8 path"),
                ],
                workspace,
            );
        },
        |_| "git push origin main".to_owned(),
        500,
        "unused",
        "The work is pushed and complete.",
    );
    assert!(end.is_success());
    assert_eq!(end.ending, RunEnding::Refused, "{end:?}");
    assert_eq!(outcome, "refused");
    let frame = card(&reason);
    assert!(frame.contains("⊘"), "{frame}");
    assert!(
        frame.contains("result: The work is pushed and complete"),
        "{frame}"
    );
}

#[test]
fn a_success_body_with_a_404_is_still_a_tick() {
    // Review correction 3, non-blocking 5: a 200 body may mention 404 without
    // being an HTTP error.
    let (end, outcome, _reason) = run_command(
        "success-404",
        |url| format!("curl -s -XDELETE {url}"),
        200,
        "error: purged 404 stale records",
        "The stale records are purged and the work is complete.",
    );
    assert_eq!(end.ending, RunEnding::Succeeded, "{end:?}");
    assert_eq!(outcome, "succeeded");
}

#[test]
fn a_refused_read_with_stderr_redirection_is_not_a_refusal() {
    // Review correction 3, non-blocking 3: `2>/dev/null` is not a write. The failed
    // GET is a failed read, not a refused instruction.
    let (end, outcome, _reason) = run_command(
        "read-redirect",
        |url| format!("curl -s -X GET {url} 2>/dev/null"),
        403,
        "Forbidden",
        "The review is complete.",
    );
    assert_eq!(end.ending, RunEnding::Succeeded, "{end:?}");
    assert_eq!(outcome, "succeeded");
}

#[test]
fn a_curl_get_with_data_is_a_read() {
    // Review correction 3, non-blocking 3: `-G` makes `--data` a query, not a body.
    let (end, outcome, _reason) = run_command(
        "get-data",
        |url| format!("curl -s -G --data 'q=1' {url}"),
        403,
        "Forbidden",
        "The search is complete.",
    );
    assert_eq!(end.ending, RunEnding::Succeeded, "{end:?}");
    assert_eq!(outcome, "succeeded");
}

#[test]
fn a_destructive_keep_success_is_a_tick() {
    // Review correction 3, non-blocking 4: a keep phrase beside a success verb for
    // the destructive act describes a success, not a refusal.
    let (end, outcome, _reason) = run_reply(
        "prose-keep-success",
        "Delete the stale branch and keep the docs as is.\n",
        "I deleted the stale branch and kept the docs as is; the work is complete.",
    );
    assert_eq!(end.ending, RunEnding::Succeeded, "{end:?}");
    assert_eq!(outcome, "succeeded");
}

#[test]
fn a_failed_write_outside_the_old_class_is_not_a_green_tick() {
    // Correction 4, blocking 2: `git branch --delete`, `git tag --delete`, `sed -i`
    // and `cp` exited non-zero while the model claimed success. The class now
    // recognises them, so the failed write is a refusal; what matters is that it is
    // never `✓`.
    for (label, command, claim) in [
        (
            "branch-delete-fail",
            "git branch --delete stale",
            "I deleted the stale branch and the work is complete.",
        ),
        (
            "tag-delete-fail",
            "git tag --delete stale",
            "I deleted the stale tag and the work is complete.",
        ),
        (
            "sed-fail",
            "sed -i s/hello/goodbye/ locked.txt",
            "The typo is fixed and the work is complete.",
        ),
        (
            "cp-fail",
            "cp README.md locked/",
            "The README is copied and the work is complete.",
        ),
    ] {
        let (end, outcome, reason) = run_scripted(
            label,
            "Finish the work.\n",
            |workspace, _base| {
                fs::write(workspace.join("README.md"), "hello\n").expect("a README");
                // A directory where sed expects a file, and a file where cp expects a
                // directory: each command fails without needing a permission change.
                fs::create_dir_all(workspace.join("locked.txt")).expect("a directory");
                fs::write(workspace.join("locked"), "not a directory\n").expect("a file");
                init_repo_with_commit(workspace);
            },
            |_| command.to_owned(),
            200,
            "unused",
            claim,
        );
        assert!(
            end.is_success(),
            "{command}: the agent process still exits 0"
        );
        assert_eq!(outcome, "refused", "{command} was not a refusal: {end:?}");
        assert!(
            reason.contains(claim.split('.').next().unwrap_or("")),
            "{command}: the model's sentence is not the result row: {reason}"
        );
        let frame = card(&reason);
        assert!(
            frame.contains("last runs ⊘") && !frame.contains("last runs ✓"),
            "{command} still drew a green run mark:\n{frame}"
        );
    }
}

#[test]
fn a_failed_destructive_write_behind_a_later_success_is_not_a_green_tick() {
    // Correction 5's blocker: the reviewer's cases 17-20. The shell exits 0 because
    // of `|| true` or the later `echo`, and the write's own failure line is not one
    // of the old phrases. The shell reports the failed segment itself.
    for (label, command, claim) in [
        (
            "compound-rm-semicolon",
            "rm missing.txt; echo done",
            "I deleted the stale file and the work is complete.",
        ),
        (
            "compound-rm-or-true",
            "rm missing.txt || true",
            "I deleted the stale file and the work is complete.",
        ),
        (
            "compound-cp-or-true",
            "cp missing.txt locked/ || true",
            "The README is copied and the work is complete.",
        ),
        (
            "compound-rmdir-or-true",
            "rmdir locked || true",
            "The directory is deleted and the work is complete.",
        ),
        (
            "compound-sed-or-true",
            "sed -i s/hello/goodbye/ locked.txt || true",
            "The typo is fixed and the work is complete.",
        ),
    ] {
        let (end, outcome, reason, failure, segments) = run_scripted_failure(
            label,
            "Delete the stale file.\n",
            |workspace, _base| {
                fs::write(workspace.join("README.md"), "hello\n").expect("a README");
                // A file where `cp` needs a directory and where `rmdir` needs an
                // empty directory; a directory where `sed -i` needs a file.
                fs::write(workspace.join("locked"), "not a directory\n").expect("a file");
                fs::create_dir_all(workspace.join("locked.txt")).expect("a directory");
            },
            |_| command.to_owned(),
            200,
            "unused",
            claim,
        );
        assert!(end.is_success(), "{command}: the wrapper exits 0");
        assert_eq!(
            outcome, "refused",
            "{command}: the failed write was not a refusal: {end:?}"
        );
        assert!(
            reason.contains(claim.split('.').next().unwrap_or("")),
            "{command}: the model's sentence is not the result row: {reason}"
        );
        assert!(
            segments.iter().any(|segment| segment.status != 0),
            "{command}: the transcript has no failed shell segment: {segments:?}"
        );
        let failure = failure.expect("the failed command is recorded for the card");
        assert!(
            failure.contains("cannot remove")
                || failure.contains("cannot stat")
                || failure.contains("failed to remove")
                || failure.contains("couldn't edit"),
            "{command}: the card's cause row lost the write's own failure: {failure}"
        );
        let frame = card_end(
            SessionEnd::Refused {
                reason: reason.clone(),
            },
            &reason,
            Some(failure),
        );
        assert!(
            frame.contains("last runs ⊘") && !frame.contains("last runs ✓"),
            "{command} still drew a green run mark:\n{frame}"
        );
        assert!(
            frame.contains("result:"),
            "{command} lost its result row:\n{frame}"
        );
        // U6/F-02: a refused write's cause row names the refusal and wears the
        // strip's `⊘`, not the read warning's `⚠ shell:`.
        assert!(
            frame.contains("⊘ write refused:"),
            "{command} lost the write-refused cause row:\n{frame}"
        );
    }
}

#[test]
fn a_failed_command_hidden_in_a_compound_is_a_warning_not_a_tick() {
    // Correction 5, rule 2: `cmd1 && cmd2` with a failing cmd2, a failed command in
    // a pipeline, and one in a command substitution. The wrapper's status is 0
    // because a later segment succeeds; the shell's per-command record decides.
    for (label, command) in [
        ("and-fails", "echo checked && ls does-not-exist"),
        ("pipeline-fails", "printf 'x\n' | grep -q zzz; echo done"),
        ("pipeline-first-fails", "false | true; echo done"),
        ("command-substitution-fails", "x=$(false); echo done"),
        ("command-substitution-swallowed", "echo $(false); echo done"),
        ("subshell-fails", "( echo sub; false ); echo done"),
    ] {
        let (end, outcome, reason, _failure, segments) = run_scripted_failure(
            label,
            "Review the work.\n",
            |_workspace, _base| {},
            |_| command.to_owned(),
            200,
            "unused",
            "The review is complete.",
        );
        assert!(end.is_success(), "{command}: the wrapper exits 0");
        assert_eq!(
            outcome, "warned",
            "{command}: a failed command was not a warning: {end:?}"
        );
        assert!(
            segments.iter().any(|segment| segment.status != 0),
            "{command}: the transcript has no failed shell segment: {segments:?}"
        );
        let frame = card_end(
            SessionEnd::Warned {
                reason: reason.clone(),
            },
            &reason,
            None,
        );
        assert!(
            frame.contains("last runs ⚠") && !frame.contains("last runs ✓"),
            "{command} still drew a green run mark:\n{frame}"
        );
        assert!(
            frame.contains("result: The review is complete"),
            "{command} lost its result row:\n{frame}"
        );
    }
}

#[test]
fn a_clean_compound_command_is_a_tick() {
    // The other direction: every command the shell reported exited 0, so the run is
    // a success even though it is a compound command.
    let (end, outcome, _reason) = run_scripted(
        "compound-clean",
        "Review the work.\n",
        |_workspace, _base| {},
        |_| "echo checked; printf 'ok\n'".to_owned(),
        200,
        "unused",
        "The review is complete.",
    );
    assert!(end.is_success(), "{end:?}");
    assert_eq!(outcome, "succeeded");
}

#[test]
fn a_failed_forgejo_read_is_a_warning_not_a_tick() {
    // Correction 5, N1: "every tool call" includes a failed read. The Forgejo is
    // unreachable on loopback; the model still claims the review is complete.
    use gewu_multiverse::run::{self, RunForgejo, RunForgejoRepository};
    let root = scratch_root("forgejo-read-fail");
    let run_dir = prepare(&root, "Review the work.\n");
    let workspace = root.join("agents").join("scribe").join("workspace");
    fs::create_dir_all(workspace.join("repo").join(".git")).expect("a cloned repository");
    mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\
          [forgejo_agent_tokens]\n\"scribe\" = \"gw-fixture-forgejo-token-not-a-real-secret\"\n",
    )
    .expect("credentials");
    run::write_forgejo_context(
        &run_dir,
        &RunForgejo {
            url: "http://127.0.0.1:1".to_owned(),
            login: "gewu-agent-1".to_owned(),
            repositories: vec![RunForgejoRepository {
                full_name: "team/repo".to_owned(),
                directory: "repo".to_owned(),
            }],
        },
    )
    .expect("the Forgejo context");
    let call = serde_json::json!({
        "model": "example-model",
        "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_read", "type": "function",
                    "function": {"name": "forgejo_list_assigned_issues",
                        "arguments": "{}"}}]}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}
    })
    .to_string();
    let model = Stub::start(vec![
        StubReply::json(call),
        StubReply::json(final_reply("The review is complete.")),
    ]);
    endpoint(&run_dir, &model.url());
    let end = agent_run::execute(&run_dir);
    let (outcome, reason) = terminal(&run_dir);
    let _ = fs::remove_dir_all(&root);
    assert!(end.is_success(), "the agent process still exits 0");
    assert_eq!(
        outcome, "warned",
        "a failed Forgejo read was not a warning: {end:?}"
    );
    assert_eq!(reason, "The review is complete.");
}

#[test]
fn a_failed_read_is_a_warning_not_a_refusal_or_a_tick() {
    // Correction 4, rule 1 + non-blocking 3A: a failed read has no refusal evidence,
    // so it is `⚠`, and `git show push` is a read of a ref, not a write.
    for (label, command) in [
        ("git-show-push", "git show push"),
        ("ls-missing", "ls does-not-exist"),
    ] {
        let (end, outcome, _reason) = run_scripted(
            label,
            "Review the work.\n",
            |workspace, _base| init_repo_with_commit(workspace),
            |_| command.to_owned(),
            200,
            "unused",
            "The review is complete.",
        );
        assert!(end.is_success(), "{command}: the agent process exits 0");
        assert_eq!(outcome, "warned", "{command}: {end:?}");
    }
}

#[test]
fn a_failed_write_behind_a_later_success_is_not_a_green_tick() {
    // Correction 4, rule 2: the wrapper exits 0 because of the later segment; the
    // failed push's own output still decides the mark.
    for (label, command) in [
        ("compound-semicolon", "git push origin main; echo done"),
        ("compound-or-true", "git push origin main || true"),
        ("compound-pipeline", "git push origin main 2>&1 | tail -5"),
    ] {
        let (end, outcome, reason) = run_scripted(
            label,
            "Push the work to the server.\n",
            |workspace, base| {
                init_repo_with_commit(workspace);
                git(
                    &["remote", "add", "origin", &format!("{base}/team/repo.git")],
                    workspace,
                );
            },
            |_| command.to_owned(),
            403,
            "Forbidden",
            "The work is pushed and complete.",
        );
        assert!(end.is_success(), "{command}: the wrapper exits 0");
        assert_eq!(outcome, "refused", "{command}: {end:?}");
        let frame = card(&reason);
        assert!(frame.contains('⊘'), "{command} is not refused:\n{frame}");
        assert!(
            frame.contains("result: The work is pushed and complete"),
            "{command} lost its result row:\n{frame}"
        );
    }
}
