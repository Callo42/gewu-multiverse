//! The correction-6 seams: the shell record's own blind spots.
//!
//! These guards are written against the public API that already existed at
//! `366493d`, so they compile on the reviewed head and fail there:
//! `agent_run::execute`, the scripted loopback model, and the terminal record. On
//! `366493d` a refused `rm` behind `if`/`while`/`!`/a function condition, or inside a
//! nested `bash -c` or `#!/bin/bash` script, was recorded with the wrapper's status 0
//! and drew a plain `✓`. On this head the shell's record keeps the condition's own
//! status, and every nested non-interactive bash installs the same recorder through
//! the product's `BASH_ENV`, so the failed write is a `⊘` refusal with its own cause.

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
        "gewu-correction-6-{label}-{}-{}",
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
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key-not-a-real-secret\"\n\
          [forgejo_agent_tokens]\n\"scribe\" = \"gw-fixture-forgejo-token-not-a-real-secret\"\n",
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

/// Make the permission-denied fixture: a directory the run owns but cannot write.
fn locked_file(workspace: &Path) {
    let locked = workspace.join("ro");
    fs::create_dir_all(&locked).expect("the read-only directory");
    fs::write(locked.join("file"), "not for deletion\n").expect("the locked file");
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
    command: &str,
    setup: impl FnOnce(&Path),
) -> (RunEnding, String, String, Vec<CommandSegment>) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, "Delete the stale file.\n");
    let workspace = root.join("agents").join("scribe").join("workspace");
    setup(&workspace);
    let model = Stub::start(vec![
        StubReply::json(tool_call(command)),
        StubReply::json(final_reply(
            "I deleted the stale file and the work is complete.",
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
fn a_denied_write_in_a_condition_is_not_a_green_tick() {
    // Correction 6, B1: `$?` at the next trap is the enclosing compound's 0 while
    // `PIPESTATUS[0]` keeps the failed condition; at 366493d all four were `✓`.
    for command in [
        "if rm ro/file; then echo removed; fi; echo done",
        "while rm ro/file; do echo x; done; echo done",
        "! rm ro/file; echo done",
        "if fs() { rm ro/file; }; fs; then echo deleted; fi; echo done",
    ] {
        let (ending, outcome, failure, segments) =
            run_command("condition-write", command, locked_file);
        assert_ne!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_ne!(
            outcome, "succeeded",
            "{command}: the card would say success"
        );
        assert!(
            failure.contains("rm: cannot remove") || failure.contains("Permission denied"),
            "{command}: the cause row does not name the refused rm: {failure:?}"
        );
        assert!(
            segments
                .iter()
                .any(|segment| segment.command.contains("rm ro/file") && segment.status != 0),
            "{command}: the condition's own failure is not in the record: {segments:?}"
        );
    }
}

#[test]
fn a_denied_write_in_a_nested_bash_is_not_a_green_tick() {
    // Correction 6, B2: `bash -c` and a `#!/bin/bash` script run in a new shell that
    // had no trap at 366493d; the write inside it was unrecorded and the outer exit 0
    // was a green tick. The product's BASH_ENV prelude installs the same recorder in
    // every nested non-interactive bash.
    for command in [
        "bash -c 'rm ro/file; echo done'",
        "bash -c 'rm ro/file'",
        "bash -c 'if rm ro/file; then echo removed; fi'",
        "bash -c 'rm ro/file' || true",
        "bash -c 'rm ro/file' | tail -1",
    ] {
        let (ending, outcome, failure, segments) =
            run_command("nested-bash-c", command, locked_file);
        assert_ne!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_ne!(
            outcome, "succeeded",
            "{command}: the card would say success"
        );
        assert!(
            failure.contains("rm: cannot remove") || failure.contains("Permission denied"),
            "{command}: the cause row does not name the refused rm: {failure:?}"
        );
        assert!(
            segments
                .iter()
                .any(|segment| segment.command.contains("rm ro/file") && segment.status != 0),
            "{command}: the nested failure is not in the record: {segments:?}"
        );
        assert!(
            segments
                .iter()
                .all(|segment| !matches!(segment.command.as_str(), "START" | "NEST" | "NESTSTART")),
            "{command}: a shell marker leaked into the record: {segments:?}"
        );
    }

    let (ending, outcome, failure, segments) =
        run_command("nested-bash-script", "bash delete.sh", |workspace| {
            locked_file(workspace);
            fs::write(
                workspace.join("delete.sh"),
                "#!/bin/bash\nrm ro/file\necho done\n",
            )
            .expect("the script");
        });
    assert_ne!(ending, RunEnding::Succeeded, "bash script: {ending:?}");
    assert_ne!(
        outcome, "succeeded",
        "bash script: the card would say success"
    );
    assert!(
        failure.contains("rm: cannot remove") || failure.contains("Permission denied"),
        "bash script: the cause row does not name the refused rm: {failure:?}"
    );
    assert!(
        segments
            .iter()
            .any(|segment| segment.command.contains("rm ro/file") && segment.status != 0),
        "bash script: the nested failure is not in the record: {segments:?}"
    );
}

#[test]
fn a_clean_compound_command_is_still_a_tick() {
    // The correction must not turn every compound command into a warning.
    let (ending, outcome, failure, _) =
        run_command("clean-compound", "echo checked; printf 'ok\\n'", |_| {});
    assert_eq!(ending, RunEnding::Succeeded, "{ending:?}");
    assert_eq!(outcome, "succeeded");
    assert!(
        failure.is_empty(),
        "a clean run has no cause row: {failure:?}"
    );
}

#[test]
fn the_cause_row_names_the_failed_write_when_two_commands_fail() {
    // Correction 5, N5: the later `ls` error must not replace the refused `rm`'s line.
    let (ending, outcome, failure, _) = run_command(
        "two-failures",
        "rm missing.txt; ls missing2; echo done",
        |_| {},
    );
    assert_eq!(ending, RunEnding::Refused, "{ending:?}");
    assert_eq!(outcome, "refused");
    assert!(
        failure.contains("rm: cannot remove"),
        "the cause row named another command: {failure:?}"
    );
}
