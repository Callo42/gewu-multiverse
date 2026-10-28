//! The correction-10 rule: the shell's own voice is the third witness.
//!
//! These guards are written against the public API that already existed at
//! `14745e9`, so they compile there and fail: `agent_run::execute`, the scripted
//! loopback model, the terminal record and the card's failure file. At `14745e9` a
//! failed `>`/`>>` redirection whose message was bash's own — an unset or glob
//! target (`ambiguous redirect`), noclobber (`cannot overwrite existing file`) —
//! left an all-zero raw record and drew a plain `✓` beside the model's false
//! success sentence. On this head the shell reports its own name through the
//! `BASH_ENV` prelude, and any line in the exact `<name>: ` prefix is shell-reported
//! trouble: a named write is `⊘`, anything else `⚠`, and the words after the prefix
//! are never matched against a list.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnding};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, CommandSegment, Record};

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-correction-10-{label}-{}-{}",
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
    let _ = fs::remove_dir_all(&root);
    (end.ending, outcome, failure, segments)
}

/// The all-zero-record shape: the redirection failure of a piped `if` condition is
/// not a raw status. At the reviewed head every case here was `✓`.
fn assert_refused(label: &str, command: &str, expected: &str, setup: impl FnOnce(&Path)) {
    let (ending, outcome, failure, segments) =
        run_command(label, "Delete the stale file.\n", command, setup);
    assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
    assert_eq!(outcome, "refused", "{command}: the card would not draw ⊘");
    assert!(
        failure.contains(expected),
        "{command}: the cause row does not name the shell's message: {failure:?}"
    );
    // The shell's own voice is the only witness: the raw record is all zero.
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn an_unset_redirection_is_a_refusal() {
    // `$n` expands to nothing: bash says `$n: ambiguous redirect`, not a C string.
    assert_refused(
        "unset-write",
        "n=; if echo x > $n; then echo wrote; fi | cat",
        "ambiguous redirect",
        |_| {},
    );
    assert_refused(
        "unset-append",
        "n=; if echo x >> $n; then echo wrote; fi | cat",
        "ambiguous redirect",
        |_| {},
    );
}

#[test]
fn a_glob_redirection_is_a_refusal() {
    // A glob with two matches is not a single target: `*.dat: ambiguous redirect`.
    assert_refused(
        "glob-write",
        "if echo x > *.dat; then echo wrote; fi | cat",
        "ambiguous redirect",
        |workspace| {
            fs::write(workspace.join("a.dat"), "a\n").expect("a.dat");
            fs::write(workspace.join("b.dat"), "b\n").expect("b.dat");
        },
    );
}

#[test]
fn a_noclobber_overwrite_is_a_refusal() {
    // `set -C` refuses to overwrite: `f: cannot overwrite existing file`.
    assert_refused(
        "noclobber-write",
        "set -C; if echo x > f; then echo wrote; fi | cat",
        "cannot overwrite existing file",
        |workspace| {
            fs::write(workspace.join("f"), "keep me\n").expect("the existing file");
        },
    );
}

#[test]
fn a_function_redirection_is_a_refusal() {
    // A function defined on the `-c` line is named `environment` by bash; the
    // product must recognise that pseudo-source as the shell's own voice.
    assert_refused(
        "function-write",
        "f() { echo x > $n; }; n=; if f; then echo wrote; fi | cat",
        "ambiguous redirect",
        |_| {},
    );
}

#[test]
fn a_case_redirection_is_a_refusal() {
    assert_refused(
        "case-write",
        "case 1 in 1) if echo x > $n; then echo wrote; fi;; esac | cat",
        "ambiguous redirect",
        |_| {},
    );
}

#[test]
fn a_nested_shells_voice_is_seen() {
    // The nested bash reports its own `$0` through the prelude. The outer command
    // (`bash -c '…'`) is not itself a recognised write, so this is `⚠`, never `✓`.
    let command = "bash -c 'n=; if echo x > $n; then echo wrote; fi | cat'";
    let (ending, outcome, failure, segments) =
        run_command("nested-voice", "Delete the stale file.\n", command, |_| {});
    assert_eq!(ending, RunEnding::Warned, "{command}: {ending:?}");
    assert_eq!(outcome, "warned", "{command}");
    assert!(
        failure.contains("ambiguous redirect"),
        "{command}: the nested shell's line is not the cause: {failure:?}"
    );
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn a_sourced_function_failure_is_seen() {
    // Bash names the file a function was defined in (`lib.sh: line 1: …`), not the
    // shell's `$0`; the trap reports `BASH_SOURCE[0]` so that name is known. The
    // command text only says `source lib.sh`, so this is `⚠`, never `✓`.
    let command = "source lib.sh; n=; if f; then echo wrote; fi | cat";
    let (ending, outcome, failure, segments) = run_command(
        "sourced-function",
        "Delete the stale file.\n",
        command,
        |workspace| {
            fs::write(workspace.join("lib.sh"), "f() { echo x > $n; }\n").expect("lib.sh");
        },
    );
    assert_eq!(ending, RunEnding::Warned, "{command}: {ending:?}");
    assert_eq!(outcome, "warned", "{command}");
    assert!(
        failure.contains("lib.sh") && failure.contains("ambiguous redirect"),
        "{command}: the sourced file's line is not the cause: {failure:?}"
    );
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn a_sourced_command_failure_is_seen() {
    // The same for a failed redirection at the top level of a sourced file, hidden
    // behind the piped condition: `top.sh: line 2: …`, all raw statuses 0.
    let command = "if source top.sh; then echo wrote; fi | cat";
    let (ending, outcome, failure, segments) = run_command(
        "sourced-command",
        "Delete the stale file.\n",
        command,
        |workspace| {
            fs::write(workspace.join("top.sh"), "n=\necho x > $n\n").expect("top.sh");
        },
    );
    assert_eq!(ending, RunEnding::Warned, "{command}: {ending:?}");
    assert_eq!(outcome, "warned", "{command}");
    assert!(
        failure.contains("top.sh") && failure.contains("ambiguous redirect"),
        "{command}: the sourced file's line is not the cause: {failure:?}"
    );
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}

#[test]
fn words_without_the_shell_prefix_are_still_a_tick() {
    // The clean control: quoting the words — or a different program's `name:` line —
    // is not the shell's voice, and a run that only read is no work.
    for command in [
        "printf '%s\\n' 'the shell said ambiguous redirect'",
        "printf '%s\\n' 'bashful: line 1: ambiguous redirect'",
        "echo done",
    ] {
        let (ending, outcome, failure, _segments) =
            run_command("clean-voice", "Review the work.\n", command, |_| {});
        assert_eq!(ending, RunEnding::NoWork, "{command}: {ending:?}");
        assert_eq!(outcome, "no_work", "{command}");
        assert!(
            failure.is_empty(),
            "{command}: a clean run has a cause row: {failure:?}"
        );
    }
}
