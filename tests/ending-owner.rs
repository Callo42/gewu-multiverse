//! Mechanical guard for the single run-ending owner.
//!
//! Every production file except the owner (`src/run.rs`) and the transcript storage
//! primitive (`src/transcript.rs`) is scanned. A terminal/stop/first-ending/
//! uncertainty write, or a raw run-process signal, outside the owner is a hard
//! failure. Server/installer process adapters are listed explicitly because they
//! signal their own child processes, not agent runs.

use std::fs;
use std::path::{Path, PathBuf};

const ENDING_TOKENS: &[&str] = &[
    "Record::RunFinished {",
    "gewu_multiverse::run::terminal_record_for_test(",
    "\"run-stopped.json\"",
    "\"first-ending.json\"",
    "\"ending-uncertain.json\"",
    "write_run_stopped",
    "write_stop_marker",
    "write_terminal_replacing",
    "ensure_terminal_record",
    "record_outcome_if_missing",
    "settle_run_with_stop",
    "settle_run(",
    "reap_leftover_run",
    "ensure_first_ending",
    "record_ending_uncertainty",
    "pin_first_ending",
    "ending_uncertainty(",
    "seal_from_storage",
    "RunHandle::new",
    "signal_starting_group",
    "recorded_first_ending(",
    "STOPPED_FILE",
    "FIRST_ENDING_FILE",
    "ENDING_UNCERTAIN_FILE",
    "stop_running_session",
];

const SIGNAL_TOKENS: &[&str] = &[
    "signal_group(",
    "killpg(",
    "nix::sys::signal::kill",
    ".handle.signal(",
];

/// Server/installer process groups, not run endings. They may use the raw signal
/// primitive on their own children; ending tokens are still banned in them.
const NON_RUN_PROCESS_FILES: &[&str] = &[
    "src/bundled_forgejo/mod.rs",
    "src/installer.rs",
    "src/editor.rs",
];

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn production_text(text: &str) -> String {
    let text = strip_comments(text);
    let marker = "#[cfg(test)]\nmod tests {";
    match text.find(marker) {
        Some(index) => text[..index].to_owned(),
        None => text,
    }
}

fn violations(relative: &str, text: &str) -> Vec<String> {
    let body = production_text(text);
    let mut banned: Vec<&str> = ENDING_TOKENS.to_vec();
    if !NON_RUN_PROCESS_FILES.contains(&relative) {
        banned.extend_from_slice(SIGNAL_TOKENS);
    }
    banned
        .into_iter()
        .filter(|token| body.contains(token))
        .map(|token| format!("`{token}` in {relative}"))
        .collect()
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.join("src")];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn only_the_owner_writes_endings_or_signals_run_handles() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    for path in source_files(root) {
        let relative = path
            .strip_prefix(root)
            .expect("a source path under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "src/run.rs" || relative == "src/transcript.rs" {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{relative} cannot be read: {error}"));
        found.extend(violations(&relative, &text));
    }
    assert!(
        found.is_empty(),
        "ending writes/signals outside the owner:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_guard_catches_a_bypassing_fixture() {
    let typed = "fn bypass(){ let _ = crate::transcript::Record::RunFinished { outcome: \"x\", reason: None, exit_code: None, duration_ms: 0, run_id: \"r\", ts: \"t\" }; }";
    assert!(!violations("src/bypass.rs", typed).is_empty());
    let aliased = "use crate::run as r; fn bypass(){ let _ = r::settle_run_with_stop(); }";
    assert!(!violations("src/bypass.rs", aliased).is_empty());
    let signal = "fn bypass(){ killpg(1, SIGTERM); }";
    assert!(!violations("src/bypass.rs", signal).is_empty());
    let literal = "fn bypass(){ let _ = \"first-ending.json\"; }";
    assert!(!violations("src/bypass.rs", literal).is_empty());
}

#[test]
fn the_owner_exposes_the_ending_requests() {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/run.rs"))
        .expect("the owner source");
    for token in [
        "pub fn prepare_ending",
        "pub fn request_stop",
        "pub fn request_force_stop",
        "pub fn stop_uncertain",
        "pub fn request_stop_and_wait",
        "pub fn request_finish",
        "pub fn recover_ending",
        "pub fn request_natural_finish",
        "pub fn request_failed_finish",
        "pub(crate) fn request_starting_stop",
        "pub(crate) fn adapter_signal_run_group",
    ] {
        assert!(text.contains(token), "the owner lacks `{token}`");
    }
}
