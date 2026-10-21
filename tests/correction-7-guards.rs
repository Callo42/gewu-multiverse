//! The correction-7 rule: the mark reads the shell's raw record, not the pairing.
//!
//! These guards are written against the public API that already existed at
//! `772689d`, so they compile on the reviewed head and fail there: `agent_run::execute`,
//! the scripted loopback model, and the terminal record. At `772689d` a refused write
//! inside an `if`/`while`/`!`/subshell/nested-bash compound **on the left of a pipeline**
//! was paired with the outer pipeline's 0 and drew a plain `✓`, although the shell's
//! record held the failure. On this head the mark reads every raw status the shell
//! reported, so no pairing defect can produce a green tick; pairing only supplies the
//! words and the `⊘`/`⚠` distinction.

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
        "gewu-correction-7-{label}-{}-{}",
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
    instruction: &str,
    command: &str,
    setup: impl FnOnce(&Path),
) -> (RunEnding, String, String, Vec<CommandSegment>, bool) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, instruction);
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
    let result = run::read_failure(&run_dir).unwrap_or_default();
    let (segments, failure) = transcript::read(&run_dir)
        .expect("the transcript reads")
        .records
        .into_iter()
        .find_map(|record| match record {
            Record::ToolResult {
                tool,
                segments,
                output,
                ..
            } if tool == "shell" => Some((segments, output)),
            _ => None,
        })
        .unwrap_or_default();
    allow_cleanup(&workspace);
    let _ = fs::remove_dir_all(&root);
    (end.ending, outcome, result, segments, failure.is_some())
}

#[test]
fn a_denied_write_in_a_pipeline_compound_is_never_a_green_tick() {
    // Review correction 6, B1: the outer pipeline's PIPESTATUS was paired with the
    // last N flat entries and replaced an already-paired failure with 0. Every one
    // of these drew a plain `✓` at 772689d.
    let commands = [
        "if rm ro/file; then echo x; fi | cat",
        "while rm ro/file; do :; done | cat",
        "(if rm ro/file; then echo x; fi) | cat",
        "if rm ro/file; then :; fi | cat > /dev/null",
        "if rm ro/file; then :; fi | true",
        "bash -c 'if rm ro/file; then echo x; fi' | cat",
        "bash -c 'rm ro/file || true' | cat",
        "bash -c '! rm ro/file' | cat",
        "if rm ro/file; then :; fi | tail -1",
        "bash -c 'if rm ro/file; then :; fi' | tail -1",
    ];
    for (index, command) in commands.iter().enumerate() {
        let (ending, outcome, failure, segments, had_output) = run_command(
            &format!("pipeline-{index}"),
            "Delete the stale file.\n",
            command,
            locked_file,
        );
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
            had_output,
            "{command}: the tool result lost the shell's output"
        );
        if command.starts_with("bash -c") {
            assert!(
                segments
                    .iter()
                    .any(|segment| segment.command.contains("rm ro/file") && segment.status != 0),
                "{command}: the nested failure is not in the paired record: {segments:?}"
            );
        }
    }
}

#[test]
fn a_nested_shell_command_is_not_listed_twice() {
    // Review correction 6, NB1: the flat reader emitted the same command twice
    // with different statuses (`rm` 1 then 0, the outer `bash` twice, `eval` 0 then
    // 1). Pairing inside each shell frame and collapsing adjacent duplicates keeps
    // one row per command.
    for command in [
        "bash -c 'rm ro/file'",
        "bash -c 'rm ro/file' | cat",
        "eval 'rm ro/file'",
        "bash -c 'rm ro/file; echo done'",
        "bash -c '! rm ro/file' | cat",
    ] {
        let (_ending, _outcome, _failure, segments, _had_output) = run_command(
            "no-duplicates",
            "Delete the stale file.\n",
            command,
            locked_file,
        );
        for pair in segments.windows(2) {
            assert_ne!(
                pair[0].command, pair[1].command,
                "{command}: the same command is listed twice: {segments:?}"
            );
        }
    }
}

#[test]
fn a_clean_compound_inside_a_pipeline_is_still_a_tick() {
    for command in [
        "if true; then echo x; fi | cat",
        "while true; do break; done | cat",
        "( echo x ) | cat",
        "bash -c 'true; echo done' | cat",
    ] {
        let (ending, outcome, failure, _segments, _had_output) =
            run_command("clean-pipeline", "Review the work.\n", command, |_| {});
        assert_eq!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_eq!(outcome, "succeeded", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}

fn quote_single(inner: &str) -> String {
    format!("'{}'", inner.replace('\'', "'\\''"))
}

fn rm_compositions(depth: usize) -> Vec<String> {
    if depth == 0 {
        return vec!["rm ro/file".to_owned()];
    }
    let mut out = Vec::new();
    for form in rm_compositions(depth - 1) {
        out.push(format!("{form}; echo ok"));
        out.push(format!("{form} && echo ok"));
        out.push(format!("{form} || true"));
        out.push(format!("{form} | cat"));
        out.push(format!("if ( {form} ); then echo ok; fi"));
        out.push(format!("while ( {form} ); do break; done"));
        out.push(format!("! ( {form} )"));
        out.push(format!("( {form} )"));
        out.push(format!("bash -c {}", quote_single(&form)));
        out.push(format!("f() {{ {form}; }}; f"));
    }
    out
}

fn clean_compositions(depth: usize) -> Vec<String> {
    if depth == 0 {
        return vec!["true".to_owned(), "echo ok".to_owned()];
    }
    let mut out = Vec::new();
    for form in clean_compositions(depth - 1) {
        out.push(format!("{form}; echo ok"));
        out.push(format!("{form} && true"));
        out.push(format!("{form} || true"));
        out.push(format!("{form} | cat"));
        out.push(format!("if ( {form} ); then echo ok; fi"));
        out.push(format!("( {form} )"));
        out.push(format!("bash -c {}", quote_single(&form)));
        out.push(format!("f() {{ {form}; }}; f"));
    }
    out
}

#[test]
fn every_generated_failing_composition_is_never_a_tick() {
    // The property: whenever the shell reports a non-zero status the mark is not
    // `✓`. `rm ro/file` always fails on the 0500 directory, and every generated
    // composition executes it, so every composition here has a raw failure.
    let mut forms = rm_compositions(2);
    // Depth 3: wrap a deterministic sample of the depth-2 forms twice more.
    for form in forms.clone().into_iter().take(10) {
        forms.push(format!("if ( {form} ); then echo ok; fi"));
        forms.push(format!("bash -c {}", quote_single(&form)));
        forms.push(format!("{form} | cat"));
    }
    assert!(forms.len() >= 120, "the generator produced too few forms");
    for (index, command) in forms.iter().enumerate() {
        let (ending, outcome, failure, _segments, _had_output) = run_command(
            &format!("generated-rm-{index}"),
            "Delete the stale file.\n",
            command,
            locked_file,
        );
        assert_ne!(
            ending,
            RunEnding::Succeeded,
            "{command}: a raw failure still drew a tick"
        );
        assert_ne!(
            outcome, "succeeded",
            "{command}: a raw failure still drew a tick"
        );
        assert!(
            !failure.is_empty(),
            "{command}: a raw failure has no cause row"
        );
    }
}

#[test]
fn every_generated_clean_composition_is_a_tick() {
    // The other half of the property: a composition made only of succeeding
    // commands is still `✓`; the rule must not turn every compound into a warning.
    let mut forms = clean_compositions(2);
    for form in forms.clone().into_iter().take(5) {
        forms.push(format!("if ( {form} ); then echo ok; fi"));
        forms.push(format!("{form} | cat"));
    }
    for (index, command) in forms.iter().enumerate() {
        let (ending, outcome, failure, _segments, _had_output) = run_command(
            &format!("generated-clean-{index}"),
            "Review the work.\n",
            command,
            |_| {},
        );
        assert_eq!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_eq!(outcome, "succeeded", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}
