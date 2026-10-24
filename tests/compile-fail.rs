//! Compile-fail proofs for the ending owner's private surface.
//!
//! A small source probe is compiled with `rustc` against the library rlib. Each
//! probe must fail to compile: terminal construction cannot forge the owner seal,
//! and the low-level settlement/force names are private.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn rlib() -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let deps = exe.parent().expect("the deps directory");
    let mut candidates: Vec<PathBuf> = fs::read_dir(deps)
        .expect("the deps directory reads")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with("libgewu_multiverse-") && name.ends_with(".rlib")
                })
        })
        .collect();
    candidates.sort_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok());
    candidates.pop().expect("the library rlib")
}

fn probe(body: &str) -> (bool, String) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "gewu-ending-compile-fail-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("probe root");
    let source = root.join("probe.rs");
    fs::write(&source, format!("extern crate gewu_multiverse;\n{body}\n")).expect("probe source");
    let deps = rlib().parent().expect("deps").to_path_buf();
    let output = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned()))
        .arg("--edition=2024")
        .arg("--crate-type=lib")
        .arg("--emit=metadata")
        .arg("--extern")
        .arg(format!("gewu_multiverse={}", rlib().display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("-o")
        .arg(root.join("probe.rmeta"))
        .arg(&source)
        .output()
        .expect("rustc runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&root);
    (output.status.success(), text)
}

fn must_fail(label: &str, body: &str) {
    let (ok, text) = probe(body);
    assert!(!ok, "{label} compiled but must fail:\n{text}");
}

#[test]
fn typed_terminal_construction_cannot_be_forged() {
    must_fail(
        "type-alias RunFinished",
        r#"
use gewu_multiverse::transcript::Record as R;
pub fn bypass() {
    let _ = R::RunFinished {
        ts: String::new(),
        run_id: String::new(),
        outcome: String::new(),
        exit_code: None,
        duration_ms: 0,
        reason: None,
    };
}
"#,
    );
    must_fail(
        "type-alias run_finished",
        r#"
use gewu_multiverse::transcript::Record as R;
pub fn bypass() {
    let _ = R::run_finished("run-1", gewu_multiverse::run::RunOutcome::Failed, None, 0, None);
}
"#,
    );
}

#[test]
fn the_seal_cannot_be_defaulted_or_reconstructed() {
    must_fail(
        "EndingSeal::default",
        "pub fn bypass() { let _ = gewu_multiverse::run::EndingSeal::default(); }",
    );
    must_fail(
        "seal_from_storage",
        "pub fn bypass() { let _ = gewu_multiverse::run::seal_from_storage(); }",
    );
    must_fail(
        "signal_starting_group",
        "pub fn bypass() { let _ = gewu_multiverse::run::signal_starting_group; }",
    );
}

#[test]
fn the_run_handle_cannot_be_signalled_outside_the_owner() {
    must_fail(
        "RunHandle::signal",
        r#"
struct H;
impl gewu_multiverse::engine::session::RunningSession for H {
    fn pid(&self) -> u32 { 1 }
    fn try_status(&mut self) -> Option<std::process::ExitStatus> { None }
    fn signal(&mut self, _force: bool) {}
}
pub fn bypass() {
    let mut handle = gewu_multiverse::run::RunHandle::new(Box::new(H));
    handle.signal(true);
}
"#,
    );
}

#[test]
fn low_level_owner_names_are_private() {
    must_fail(
        "settle_run_with_stop",
        "pub fn bypass() { let _ = gewu_multiverse::run::settle_run_with_stop; }",
    );
    must_fail(
        "request_force",
        "pub fn bypass() { let _ = gewu_multiverse::run::request_force; }",
    );
    must_fail(
        "PreparedRun::create",
        "pub fn bypass() { let _ = gewu_multiverse::run::PreparedRun::create; }",
    );
    must_fail(
        "PreparedRun::disarm",
        "pub fn bypass() { let _ = gewu_multiverse::run::PreparedRun::disarm; }",
    );
    must_fail(
        "settle_ending",
        "pub fn bypass() { let _ = gewu_multiverse::run::settle_ending; }",
    );
    must_fail(
        "write_natural_ending",
        "pub fn bypass() { let _ = gewu_multiverse::run::write_natural_ending; }",
    );
    must_fail(
        "write_failed_ending",
        "pub fn bypass() { let _ = gewu_multiverse::run::write_failed_ending; }",
    );
    must_fail(
        "signal_run_group",
        "pub fn bypass() { let _ = gewu_multiverse::run::signal_run_group; }",
    );
    must_fail(
        "ensure_first_ending",
        "pub fn bypass() { let _ = gewu_multiverse::run::ensure_first_ending; }",
    );
}
