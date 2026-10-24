//! Mechanical guard for the single run-ending owner.
//!
//! Production code outside `src/run.rs` may request an ending, but it must not
//! write terminal/stop/first-ending/uncertainty records or signal a run handle
//! itself. Tests are excluded: they intentionally construct records directly.

use std::fs;
use std::path::Path;

fn production(path: &str) -> String {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("{path} cannot be read: {error}"));
    let marker = "#[cfg(test)]\nmod tests {";
    match text.find(marker) {
        Some(index) => text[..index].to_owned(),
        None => text,
    }
}

#[test]
fn only_the_owner_writes_endings_or_signals_run_handles() {
    let forbidden = [
        "run::ensure_first_ending",
        "run::settle_run_with_stop",
        "run::reap_leftover_run",
        "run::write_run_stopped",
        "run::record_ending_uncertainty",
        "run::write_stop_marker",
        "Record::run_finished",
        "STOPPED_FILE",
        "FIRST_ENDING_FILE",
        "ENDING_UNCERTAIN_FILE",
        ".handle.signal(",
        "stop_running_session",
    ];
    for path in ["src/engine/mod.rs", "src/agent_run.rs", "src/main.rs"] {
        let text = production(path);
        for token in forbidden {
            assert!(
                !text.contains(token),
                "`{token}` is used outside the run-ending owner in {path}"
            );
        }
    }
}

#[test]
fn the_owner_exposes_the_ending_requests() {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/run.rs"))
        .expect("the owner source");
    for token in [
        "pub fn request_stop",
        "pub fn request_force",
        "pub fn stop_uncertain",
        "pub fn stop_and_wait",
        "pub fn settle_ending",
        "pub fn recover_ending",
        "pub fn write_natural_ending",
        "pub fn write_failed_ending",
    ] {
        assert!(text.contains(token), "the owner lacks `{token}`");
    }
}
