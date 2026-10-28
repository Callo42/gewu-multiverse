//! The correction-8 rule: the operating system's own diagnostic is a second witness.
//!
//! These guards are written against the public API that already existed at
//! `debc402`, so they compile on the reviewed head and fail there: `agent_run::execute`,
//! the scripted loopback model and the terminal record. At `debc402` a failed `>`
//! redirection inside a piped `if`/`while` condition was a plain `✓`: the shell's
//! raw record is all zero for that shape (the condition's status is the enclosing
//! compound's 0), and the old fallback could only name a program's own line, while a
//! redirection has no program. On this head a POSIX diagnostic line in the tool
//! output lowers the mark to `⊘` for a recognised write (and `⚠` for any other
//! program), whatever the raw record says.

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
        "gewu-correction-8-{label}-{}-{}",
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

/// A directory the run owns but cannot write into, holding a file it must not
/// overwrite. Both the existing file and a new path in the directory are targets.
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
) -> (RunEnding, String, String, Vec<CommandSegment>, bool, String) {
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
    let result = run::read_failure(&run_dir).unwrap_or_default();
    let (segments, failure, output) = transcript::read(&run_dir)
        .expect("the transcript reads")
        .records
        .into_iter()
        .find_map(|record| match record {
            Record::ToolResult {
                tool,
                segments,
                output,
                ..
            } if tool == "shell" => Some((segments, output, String::new())),
            _ => None,
        })
        .map(|(segments, output, _)| (segments, result, output.unwrap_or_default()))
        .unwrap_or_default();
    allow_cleanup(&workspace);
    let _ = fs::remove_dir_all(&root);
    (
        end.ending,
        outcome,
        failure,
        segments,
        !output.is_empty(),
        output,
    )
}

#[test]
fn a_failed_redirection_in_a_piped_condition_is_a_refusal() {
    // The reviewer's five shapes at `debc402`: the raw record is all zero, the only
    // witness is the shell's own `Permission denied`, and the run drew `✓`.
    let commands = [
        "if echo x > ro/file; then echo wrote; fi | cat",
        "if echo x > ro/newfile; then echo wrote; fi | cat",
        "while echo x > ro/file; do break; done | cat",
        "if echo x > ro/file; then echo wrote; fi | tail -1",
        "if echo x > ro/file; then echo wrote; fi | cat > /dev/null",
    ];
    for (index, command) in commands.iter().enumerate() {
        let (ending, outcome, failure, segments, had_output, output) = run_command(
            &format!("redirection-{index}"),
            "Delete the stale file.\n",
            command,
            locked_file,
        );
        assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
        assert_eq!(outcome, "refused", "{command}: the card would not draw ⊘");
        assert!(
            failure.contains("Permission denied") && failure.contains("ro/"),
            "{command}: the cause row does not name the refused write: {failure:?}"
        );
        assert!(
            had_output && output.contains("Permission denied"),
            "{command}: the transcript lost the shell's diagnostic: {output:?}"
        );
        // The raw record really is all zero for this shape; the guard is for the
        // second witness, not a pairing change.
        assert!(
            segments.iter().all(|segment| segment.status == 0),
            "{command}: this shape should carry no non-zero segment: {segments:?}"
        );
    }
}

#[test]
fn the_new_target_in_a_locked_directory_is_also_a_refusal() {
    for command in [
        "if echo x > ro/newfile; then echo wrote; fi | cat",
        "if printf x > ro/newfile; then echo wrote; fi | tail -1",
    ] {
        let (ending, outcome, failure, _segments, _had_output, _output) = run_command(
            "redirection-new",
            "Delete the stale file.\n",
            command,
            locked_file,
        );
        assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
        assert_eq!(outcome, "refused", "{command}");
        assert!(
            failure.contains("Permission denied") && failure.contains("ro/newfile"),
            "{command}: the cause row does not name the new target: {failure:?}"
        );
    }
}

#[test]
fn an_unlisted_program_failing_under_a_piped_condition_is_never_a_tick() {
    // The diagnostic is the system's, not a program list: `install` is not one of the
    // product's write programs and the command has no redirection, so the honest
    // mark is `⚠`, never `✓`.
    let command = "if install -m 600 a ro/; then :; fi | cat";
    let (ending, outcome, failure, _segments, _had_output, output) = run_command(
        "unlisted-program",
        "Delete the stale file.\n",
        command,
        |workspace| {
            fs::write(workspace.join("a"), "source\n").expect("source file");
            locked_file(workspace);
        },
    );
    assert_ne!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
    assert_eq!(ending, RunEnding::Warned, "{command}: {ending:?}");
    assert_eq!(outcome, "warned", "{command}");
    assert!(
        failure.contains("Permission denied") || failure.contains("cannot create"),
        "{command}: the cause row does not name the diagnostic: {failure:?}"
    );
    assert!(
        output.contains("install:"),
        "{command}: the transcript lost the program's line: {output:?}"
    );
}

#[test]
fn a_clean_command_whose_output_quotes_an_error_is_no_work() {
    // A false `⚠` is accepted (boundary c), but an ordinary success is not lowered
    // without the POSIX diagnostic form.
    for command in [
        "echo checked",
        "echo 'Permission denied'",
        "printf 'checked\\nFile exists\\n'",
    ] {
        let (ending, outcome, failure, _segments, _had_output, _output) =
            run_command("clean-diagnostic", "Review the work.\n", command, |_| {});
        assert_eq!(ending, RunEnding::NoWork, "{command}: {ending:?}");
        assert_eq!(outcome, "no_work", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}

fn quote_single(inner: &str) -> String {
    format!("'{}'", inner.replace('\'', "'\\''"))
}

/// Failing leaves: a write the operating system refuses, in both the existing-file
/// and new-target forms new in correction 8.
fn redirection_leaves() -> Vec<String> {
    vec![
        "echo x > ro/file".to_owned(),
        "printf x > ro/newfile".to_owned(),
        "if echo x > ro/file; then echo wrote; fi".to_owned(),
        "while printf x > ro/newfile; do break; done".to_owned(),
    ]
}

fn redirection_compositions(depth: usize) -> Vec<String> {
    if depth == 0 {
        return redirection_leaves();
    }
    let mut out = Vec::new();
    for form in redirection_compositions(depth - 1) {
        out.push(format!("{form}; echo ok"));
        out.push(format!("{form} || true"));
        out.push(format!("{form} | cat"));
        out.push(format!("if ( {form} ); then echo ok; fi"));
        out.push(format!("while ( {form} ); do break; done"));
        out.push(format!("( {form} ) | cat"));
        out.push(format!("bash -c {}", quote_single(&form)));
        out.push(format!("bash -c {} | cat", quote_single(&form)));
        out.push(format!("{form} | tail -1"));
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
        out.push(format!("{form} | cat"));
        out.push(format!("if ( {form} ); then echo ok; fi"));
        out.push(format!("( {form} )"));
        out.push(format!("bash -c {}", quote_single(&form)));
    }
    out
}

#[test]
fn every_generated_redirection_composition_is_never_a_tick() {
    // The property: a generated composition that redirects into a read-only target
    // never draws a tick, even when every raw status is 0 (the piped-condition
    // shape), and always carries a cause row.
    let mut forms = redirection_compositions(2);
    for form in forms.clone().into_iter().take(8) {
        forms.push(format!("if ( {form} ); then echo ok; fi | cat"));
        forms.push(format!("{form} | cat > /dev/null"));
        forms.push(format!("bash -c {} | tail -1", quote_single(&form)));
    }
    assert!(forms.len() >= 100, "the generator produced too few forms");
    for (index, command) in forms.iter().enumerate() {
        let (ending, outcome, failure, _segments, _had_output, _output) = run_command(
            &format!("generated-redirection-{index}"),
            "Delete the stale file.\n",
            command,
            locked_file,
        );
        assert_ne!(
            ending,
            RunEnding::Succeeded,
            "{command}: a refused redirection still drew a tick"
        );
        assert_ne!(
            outcome, "succeeded",
            "{command}: a refused redirection still drew a tick"
        );
        assert!(
            !failure.is_empty(),
            "{command}: a refused redirection has no cause row"
        );
        assert!(
            failure.contains("Permission denied")
                || failure.contains("cannot create")
                || failure.contains("cannot remove")
                || failure.contains("Read-only file system"),
            "{command}: the cause row is not the system diagnostic: {failure:?}"
        );
    }
}

#[test]
fn every_generated_clean_composition_is_no_work() {
    for (index, command) in clean_compositions(2).iter().enumerate() {
        let (ending, outcome, failure, _segments, _had_output, _output) = run_command(
            &format!("generated-clean-{index}"),
            "Review the work.\n",
            command,
            |_| {},
        );
        assert_eq!(ending, RunEnding::NoWork, "{command}: {ending:?}");
        assert_eq!(outcome, "no_work", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}
