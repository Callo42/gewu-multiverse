//! The binary's command line: the real board, the scripted `--demo`, the Forgejo
//! installer, and the one-line refusals for a terminal or an unknown argument.

use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_gewu-multiverse")
}

#[test]
fn without_demo_the_real_board_refuses_a_missing_terminal_in_one_line() {
    let output = Command::new(binary())
        .env("HOME", std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn help_and_version_still_answer_without_a_terminal() {
    let help = Command::new(binary())
        .arg("--help")
        .output()
        .expect("the binary runs");
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("--demo"), "{text}");
    assert!(text.contains("--install-forgejo"), "{text}");
    assert!(text.contains("BoardView"), "{text}");

    let version = Command::new(binary())
        .arg("--version")
        .output()
        .expect("the binary runs");
    assert!(version.status.success());
    let text = String::from_utf8_lossy(&version.stdout);
    assert!(text.starts_with("gewu-multiverse "), "{text}");
}

#[test]
fn an_unknown_argument_is_refused_in_one_line() {
    let output = Command::new(binary())
        .arg("--engine")
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("not accepted"), "{stderr}");
}

#[test]
fn demo_refuses_a_run_without_a_terminal_instead_of_hanging() {
    let output = Command::new(binary())
        .arg("--demo")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
}
