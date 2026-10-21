//! The correction-5 seam: the shell reports each command's own status.
//!
//! These three guards are deliberately written against the public API that already
//! existed at `409109e`, so they compile on the reviewed head and fail there:
//! `agent_run::execute`, the scripted loopback model, and the terminal record. On
//! this head the shell's private status record makes each a warning or a refusal; on
//! `409109e` each was a green `✓` because the failure was behind a later successful
//! segment and its output did not contain one of the old phrases (the reviewer's
//! cases 17-20 and the pipeline/command-substitution cases).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{self, RunEnding};
use gewu_multiverse::mode;
use gewu_multiverse::run::{self, RunForgejo, RunForgejoRepository, RunPlan};
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript::{self, Record};

fn scratch_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-correction-5-{label}-{}-{}",
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

fn run_command(label: &str, command: &str) -> (RunEnding, String) {
    run_command_with(label, "Delete the stale file.\n", |_| {}, command)
}

fn run_command_with(
    label: &str,
    instruction: &str,
    setup: impl FnOnce(&Path),
    command: &str,
) -> (RunEnding, String) {
    let root = scratch_root(label);
    let run_dir = prepare(&root, instruction);
    let workspace = root.join("agents").join("scribe").join("workspace");
    setup(&workspace);
    let model = Stub::start(vec![
        StubReply::json(tool_call(command)),
        StubReply::json(final_reply("The work is deleted and complete.")),
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
    let _ = fs::remove_dir_all(&root);
    (end.ending, outcome)
}

#[test]
fn a_failed_write_behind_a_later_success_is_not_a_green_tick() {
    // Reviewer case 19: `rm missing.txt || true` ended `succeeded` at 409109e.
    for command in [
        "rm missing.txt; echo done",
        "rm missing.txt || true",
        "cp missing.txt locked/ || true",
        "rmdir locked || true",
    ] {
        let (ending, outcome) = run_command_with(
            "compound-write",
            "Delete the stale file.\n",
            |workspace| {
                fs::write(workspace.join("locked"), "not a directory\n").expect("a file");
            },
            command,
        );
        assert_ne!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_ne!(
            outcome, "succeeded",
            "{command}: the card would say success"
        );
    }
}

#[test]
fn a_failed_command_hidden_in_a_compound_is_not_a_green_tick() {
    // A failed `&&` branch, a failed pipeline element and a failed command
    // substitution all kept the wrapper's 0 at 409109e.
    for command in [
        "echo checked && ls does-not-exist",
        "false | true; echo done",
        "x=$(false); echo done",
        "echo $(false); echo done",
    ] {
        let (ending, outcome) = run_command("compound-hidden", command);
        assert_ne!(ending, RunEnding::Succeeded, "{command}: {ending:?}");
        assert_ne!(
            outcome, "succeeded",
            "{command}: the card would say success"
        );
    }
}

#[test]
fn a_failed_forgejo_read_is_not_a_green_tick() {
    // Correction 5, N1: a failed read is a failed call. At 409109e it was `✓`.
    let root = scratch_root("forgejo-read");
    let run_dir = prepare(&root, "Review the work.\n");
    let workspace = root.join("agents").join("scribe").join("workspace");
    fs::create_dir_all(workspace.join("repo").join(".git")).expect("a cloned repository");
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
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        end.ending,
        RunEnding::Warned,
        "a failed Forgejo read was a success at 409109e"
    );
    assert_eq!(outcome, "warned");
}
