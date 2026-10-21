//! The correction-11 guard: a reported shell name is not capped at 512 bytes.
//!
//! The private recorder reports the name bash uses in its own diagnostics. A Linux
//! path may be up to `PATH_MAX` (4096), so a 607-byte script path is a real name; at
//! `c3bd7d5` the reader dropped it at 512 bytes and the refused overwrite inside the
//! script left a plain `✓` with the model's false success sentence. On this head the
//! 607-byte name is read, its line is the shell's own voice, and the run is never
//! `✓`; the card carries the diagnostic as its cause row.

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
        "gewu-correction-11-{label}-{}-{}",
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

/// The reviewer's shape: three 200-byte components and a script whose noclobber
/// overwrite is refused inside a piped `if`.
fn long_relative() -> String {
    let part = "a".repeat(200);
    format!("{part}/{part}/{part}/s.sh")
}

#[test]
fn a_long_script_name_is_the_shells_own_voice() {
    let relative = long_relative();
    assert_eq!(relative.len(), 607, "the reviewer's path length");
    let command = format!("if bash {relative}; then echo wrote; fi | cat");
    let (ending, outcome, failure, segments) = run_command(
        "long-script",
        "Delete the stale file.\n",
        &command,
        |workspace| {
            let script = workspace.join(&relative);
            fs::create_dir_all(script.parent().expect("the script's parent"))
                .expect("the long directory tree");
            fs::write(
                &script,
                "set -C\nif echo x > f; then echo wrote; fi | cat\n",
            )
            .expect("the script");
            fs::write(workspace.join("f"), "keep me\n").expect("the existing file");
        },
    );
    assert_eq!(ending, RunEnding::Refused, "{command}: {ending:?}");
    assert_eq!(outcome, "refused", "{command}: the card would not draw ⊘");
    assert!(
        failure.contains("s.sh") && failure.contains("cannot overwrite existing file"),
        "{command}: the shell's line is not the cause: {failure:?}"
    );
    // The shell's raw record is all zero; the 607-byte name is the only witness.
    assert!(
        segments.iter().all(|segment| segment.status == 0),
        "{command}: this shape should carry no non-zero segment: {segments:?}"
    );
}
