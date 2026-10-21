//! The correction-9 rule: the diagnostic floor is the C library's whole vocabulary.
//!
//! These guards are written against the public API that existed at `7b6c139`, so they
//! compile there and fail: `agent_run::execute`, the scripted loopback model, the
//! terminal record and the card's failure file. At `7b6c139` a failed `>` redirection
//! whose system error string was outside a hand-kept nine-string list — a symlink
//! loop (`Too many levels of symbolic links`) or a too-long name (`File name too
//! long`) — was a plain `✓` beside the model's false success sentence. At this head
//! the C library's own error vocabulary is the second witness: a POSIX diagnostic
//! lowers the mark to `⊘` for a recognised write, and a mixed failure names the
//! failed write's line rather than a read's later one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnding};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, CommandSegment, Record};

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-correction-9-{label}-{}-{}",
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

fn locked_file(workspace: &Path) {
    let locked = workspace.join("ro");
    fs::create_dir_all(&locked).expect("the read-only directory");
    let file = locked.join("file");
    fs::write(&file, "not for deletion\n").expect("the locked file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).expect("the file mode");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("the directory mode");
}

fn allow_cleanup(workspace: &Path) {
    let locked = workspace.join("ro");
    if locked.is_dir() {
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));
    }
}

fn run_command(
    label: &str,
    instruction: &str,
    command: &str,
    setup: impl FnOnce(&Path),
) -> (RunEnding, String, String, Vec<CommandSegment>) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, instruction);
    let workspace = root.join("agents").join("scribe").join("workspace");
    setup(&workspace);
    let model = Stub::start(vec![
        StubReply::json(tool_call(command)),
        StubReply::json(final_reply(
            "I cleared the stale file and the work is complete.",
        )),
    ]);
    let mut plan = RunPlan::read(&run_dir).expect("the plan reads");
    plan.endpoint = model.url();
    plan.write(&run_dir).expect("the plan is rewritten");
    let end = agent_run::execute(&run_dir);
    let outcome = transcript::terminal_record(&run_dir)
        .and_then(|record| match record {
            Record::RunFinished { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("the terminal record");
    let failure = run::read_failure(&run_dir).unwrap_or_default();
    let segments = transcript::read(&run_dir)
        .expect("the transcript reads")
        .records
        .into_iter()
        .find_map(|record| match record {
            Record::ToolResult { tool, segments, .. } if tool == "shell" => Some(segments),
            _ => None,
        })
        .unwrap_or_default();
    allow_cleanup(&workspace);
    let _ = fs::remove_dir_all(&root);
    (end.ending, outcome, failure, segments)
}

#[test]
fn a_symlink_loop_redirection_is_a_refusal() {
    // `loop -> loop`: the shell prints `Too many levels of symbolic links`, outside
    // the nine-string list at `7b6c139`; the raw record is all zero for the piped
    // condition, so the run drew `✓`.
    let command = "if echo x > loop; then echo wrote; fi | cat";
    let (ending, outcome, failure, segments) = run_command(
        "symlink-loop",
        "Delete the stale file.\n",
        command,
        |workspace| {
            std::os::unix::fs::symlink("loop", workspace.join("loop")).expect("the symlink loop");
        },
    );
    assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
    assert_eq!(outcome, "refused", "{command}: the card would not draw ⊘");
    assert!(
        failure.contains("Too many levels of symbolic links"),
        "{command}: the cause row does not name the system refusal: {failure:?}"
    );
    // The second witness is the diagnostic, not a status change.
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn a_too_long_name_redirection_is_a_refusal() {
    // A 300-character file name: the shell prints `File name too long`, again outside
    // the old list and with an all-zero raw record.
    let command = "n=$(printf 'a%.0s' {1..300}); if echo x > \"$n\"; then echo wrote; fi | cat";
    let (ending, outcome, failure, segments) =
        run_command("too-long-name", "Delete the stale file.\n", command, |_| {});
    assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
    assert_eq!(outcome, "refused", "{command}: the card would not draw ⊘");
    assert!(!failure.is_empty(), "{command}: a refusal has no cause row");
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn a_mixed_failure_names_the_failed_write() {
    // Review correction 9, NB-A: the read's line is later in the output, but the
    // card's cause is the failed write's own `ro/file: Permission denied`.
    let command = "if { cat missing.txt; echo x > ro/file; }; then echo wrote; fi | cat";
    let (ending, outcome, failure, _segments) = run_command(
        "mixed-write",
        "Delete the stale file.\n",
        command,
        locked_file,
    );
    assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
    assert_eq!(outcome, "refused", "{command}");
    assert!(
        failure.contains("ro/file") && failure.contains("Permission denied"),
        "{command}: the cause does not name the failed write: {failure:?}"
    );
    assert!(
        !failure.contains("missing.txt"),
        "{command}: the cause is the read's line, not the write's: {failure:?}"
    );
}

#[test]
fn a_clean_command_quoting_a_system_error_is_still_a_tick() {
    // A false `⚠` is accepted (boundary c), but an ordinary success is not lowered
    // without the POSIX diagnostic form, even when it quotes the full vocabulary.
    for command in [
        "echo checked",
        "echo 'Too many levels of symbolic links'",
        "printf 'File name too long\\n'",
    ] {
        let (ending, outcome, failure, _segments) =
            run_command("clean-diagnostic", "Review the work.\n", command, |_| {});
        assert_eq!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_eq!(outcome, "succeeded", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}
