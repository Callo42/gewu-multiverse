//! Actual installer paths: slow downloads, cancellation, checksum and archive
//! failures, hard kills before and after publication, stale and foreign stages,
//! parallel requests, missing Node/npm and blocked DeepSeek Harness helpers.

mod support;

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use gewu_multiverse::harness::{self, Program};
use support::{
    Sandbox, dsh_old_install, node_platform, pi_archive, pi_old_install, sha256_hex, signal,
    stage_dirs, wait_until, write_executable,
};

fn run(
    sandbox: &Sandbox,
    program: &str,
    server: &support::Server,
    digest: &str,
) -> std::process::Output {
    let mut command = sandbox.product();
    server.set_on(&mut command, digest);
    command
        .args(["--install-harness", program])
        .output()
        .expect("the product runs")
}

#[test]
fn a_checksum_and_an_invalid_archive_keep_the_old_install() {
    let sandbox = Sandbox::new("checksum");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");

    let body = b"this is not the Pi package".to_vec();
    let server = support::Server::body(body.clone(), 64 * 1024, Duration::ZERO);
    let output = run(&sandbox, "pi", &server, &"0".repeat(64));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );
    assert!(root.join("install.json").is_file());
    assert!(stage_dirs(&sandbox.harness_dir("pi")).is_empty());
    assert!(!sandbox.harness_dir("pi").join(".install-journal").exists());

    let server = support::Server::body(body.clone(), 64 * 1024, Duration::ZERO);
    let digest = sha256_hex(&body);
    let output = run(&sandbox, "pi", &server, &digest);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid package archive"), "{stderr}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );
    assert!(stage_dirs(&sandbox.harness_dir("pi")).is_empty());
}

#[test]
fn a_missing_node_or_npm_is_refused_before_a_stage_is_built() {
    let sandbox = Sandbox::new("missing-node");
    let root = sandbox.install_root("dsh", "0.2.1-rc.2");
    dsh_old_install(&root, "0.2.0", 0);
    let old = fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint");
    let mut command = sandbox.product();
    command
        .env("PATH", &sandbox.bin)
        .args(["--install-harness", "dsh"]);
    let output = command.output().expect("the product runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Node.js 24.9 or newer and npm"), "{stderr}");
    assert_eq!(
        fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint"),
        old
    );
    assert!(stage_dirs(&sandbox.harness_dir("dsh")).is_empty());
}

#[test]
fn cancelling_a_slow_download_keeps_the_old_install_and_removes_the_stage() {
    let sandbox = Sandbox::new("cancel");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let body = vec![7u8; 2 * 1024 * 1024];
    let server = support::Server::body(body, 4096, Duration::from_millis(10));
    let mut command = sandbox.product();
    server.set_on(&mut command, &sha256_hex(&vec![7u8; 2 * 1024 * 1024]));
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let ready = wait_until(
        || {
            stage_dirs(&harness).iter().any(|stage| {
                fs::metadata(stage.join("pi.tar.part"))
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
            })
        },
        Duration::from_secs(10),
    );
    assert!(ready, "the slow download started");
    signal(&child, "INT");
    let status = child.wait().expect("the installer ends");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );
    assert!(stage_dirs(&harness).is_empty());
    assert!(!harness.join(".install-journal").exists());
}

#[test]
fn cancelling_during_extraction_kills_the_unpacker_and_removes_the_stage() {
    let sandbox = Sandbox::new("cancel-tar");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let archive = pi_archive(&sandbox.root);
    let server = support::Server::body(archive.clone(), 64 * 1024, Duration::ZERO);
    let marker = sandbox.root.join("tar-started");
    write_executable(
        &sandbox.bin.join("tar"),
        "#!/bin/sh\n: > \"$TAR_STARTED\"\nexec sleep 30\n",
    );
    let mut command = sandbox.product();
    server.set_on(&mut command, &sha256_hex(&archive));
    let mut child = command
        .env("TAR_STARTED", &marker)
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    assert!(
        wait_until(|| marker.exists(), Duration::from_secs(10)),
        "the unpacker did not start"
    );
    signal(&child, "INT");
    let status = child.wait().expect("the installer ends");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );
    assert!(stage_dirs(&sandbox.harness_dir("pi")).is_empty());
}

#[test]
fn cancelling_during_the_locked_package_build_kills_npm_and_removes_the_stage() {
    let sandbox = Sandbox::new("cancel-npm");
    let root = sandbox.install_root("dsh", "0.2.1-rc.2");
    dsh_old_install(&root, "0.2.0", 0);
    let old = fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint");
    write_executable(
        &sandbox.bin.join("npm"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf '11.17.0
'; exit 0; fi
if [ "$1" = "config" ]; then printf 'false
'; exit 0; fi
: > "$NPM_STARTED"
exec sleep 30
"#,
    );
    let marker = sandbox.root.join("npm-started");
    let mut command = sandbox.product();
    let mut child = command
        .env("NPM_STARTED", &marker)
        .args(["--install-harness", "dsh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    assert!(
        wait_until(|| marker.exists(), Duration::from_secs(10)),
        "npm did not start"
    );
    signal(&child, "INT");
    let status = child.wait().expect("the installer ends");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert_eq!(
        fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint"),
        old
    );
    assert!(stage_dirs(&sandbox.harness_dir("dsh")).is_empty());
}

#[test]
fn a_hard_kill_before_the_rename_leaves_only_an_owned_stage_and_a_foreign_stage_survives_the_next_install()
 {
    let sandbox = Sandbox::new("kill-before");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let body = vec![9u8; 2 * 1024 * 1024];
    let server = support::Server::body(body.clone(), 4096, Duration::from_millis(10));
    let mut command = sandbox.product();
    server.set_on(&mut command, &sha256_hex(&body));
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let ready = wait_until(
        || {
            stage_dirs(&harness).iter().any(|stage| {
                fs::metadata(stage.join("pi.tar.part"))
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
            })
        },
        Duration::from_secs(10),
    );
    assert!(ready, "the download started");
    child.kill().expect("SIGKILL");
    let status = child.wait().expect("the installer is reaped");
    assert_eq!(status.signal(), Some(9));
    assert!(!stage_dirs(&harness).is_empty(), "the owned stage is left");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );

    let foreign = harness.join(".stage-foreign");
    fs::create_dir_all(&foreign).expect("the foreign stage");
    fs::write(foreign.join("keep"), b"user data").expect("the foreign file");
    let foreign_old = harness.join(".old-foreign");
    fs::create_dir_all(&foreign_old).expect("the foreign backup");
    fs::write(foreign_old.join("keep"), b"backup data").expect("the foreign backup file");
    // A prefixed legacy stage with only the package files the old installer wrote is
    // recognised; a foreign directory with the same name shape is kept.
    let legacy = harness.join("1.4.part-999");
    fs::create_dir_all(&legacy).expect("the legacy stage");
    fs::write(legacy.join("pi.tar.part"), b"partial").expect("the legacy part");
    let legacy_foreign = harness.join("1.4.part-998");
    fs::create_dir_all(&legacy_foreign).expect("the foreign legacy-shaped stage");
    fs::write(legacy_foreign.join("keep"), b"user data").expect("the foreign legacy file");

    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let output = run(&sandbox, "pi", &server, &digest);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(foreign.join("keep"))
            .expect("the foreign file")
            .contains("user data")
    );
    assert!(stage_dirs(&harness).iter().all(|stage| stage == &foreign));
    assert!(
        fs::read_to_string(foreign_old.join("keep"))
            .expect("the foreign backup")
            .contains("backup data")
    );
    assert!(!legacy.exists(), "an owned legacy stage was not recovered");
    assert!(
        fs::read_to_string(legacy_foreign.join("keep"))
            .expect("the foreign legacy file")
            .contains("user data")
    );
    assert!(!harness.join(".install-journal").exists());
    assert!(
        fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
            .all(|entry| entry.path() == foreign_old)
    );
}

#[test]
fn a_hard_kill_after_the_old_root_rename_is_recovered_before_use() {
    let sandbox = Sandbox::new("kill-after");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    command.env("GEWU_INSTALL_TEST_PAUSE", "after-old-rename");
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let journal = harness.join(".install-journal");
    let ready = wait_until(
        || {
            fs::read_to_string(&journal)
                .map(|text| text.contains("\"old-moved\""))
                .unwrap_or(false)
                && !root.exists()
        },
        Duration::from_secs(15),
    );
    assert!(
        ready,
        "the installer reached the pause after the old rename"
    );
    child.kill().expect("SIGKILL");
    let status = child.wait().expect("the installer is reaped");
    assert_eq!(status.signal(), Some(9));
    assert!(!root.exists());
    assert!(journal.exists(), "the journal is durable");
    assert!(!stage_dirs(&harness).is_empty());
    assert!(
        fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );

    let notes = harness::recover_pending(&sandbox.data_root());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the old binary"),
        old_binary
    );
    assert!(!journal.exists());
    assert!(stage_dirs(&harness).is_empty());
    assert!(
        !fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
}

#[test]
fn cancelling_after_the_old_rename_restores_it_and_removes_the_journal() {
    let sandbox = Sandbox::new("cancel-after-old");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    command.env("GEWU_INSTALL_TEST_PAUSE", "after-old-rename");
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let journal = harness.join(".install-journal");
    let ready = wait_until(
        || {
            fs::read_to_string(&journal)
                .map(|text| text.contains("\"old-moved\""))
                .unwrap_or(false)
                && !root.exists()
        },
        Duration::from_secs(15),
    );
    assert!(
        ready,
        "the installer reached the pause after the old rename"
    );
    signal(&child, "INT");
    let status = child.wait().expect("the installer ends");
    assert_eq!(status.code(), Some(130), "{status:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the restored old binary"),
        old_binary
    );
    assert!(!journal.exists());
    assert!(stage_dirs(&harness).is_empty());
    assert!(
        !fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
}

#[test]
fn a_failed_restore_keeps_foreign_data_the_old_backup_and_the_journal() {
    let sandbox = Sandbox::new("failed-restore");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    command.env("GEWU_INSTALL_TEST_PAUSE", "after-old-rename");
    let child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let journal = harness.join(".install-journal");
    let ready = wait_until(
        || {
            fs::read_to_string(&journal)
                .map(|text| text.contains("\"old-moved\""))
                .unwrap_or(false)
                && !root.exists()
        },
        Duration::from_secs(15),
    );
    assert!(
        ready,
        "the installer reached the pause after the old rename"
    );

    // Occupy the destination with data this installer does not own. A truthful
    // rollback must leave it alone and keep the old backup and journal.
    fs::create_dir_all(&root).expect("the foreign root");
    fs::write(root.join("foreign-keep"), b"user data").expect("the foreign data");
    signal(&child, "INT");
    let output = child.wait_with_output().expect("the installer ends");
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not be confirmed restored")
            && stderr.contains("recovery journal")
            && stderr.contains("owned backup were kept"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(root.join("foreign-keep")).expect("the foreign data"),
        b"user data"
    );
    assert!(journal.exists(), "the recovery journal was removed");
    assert!(
        fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
    assert!(stage_dirs(&harness).is_empty());

    // Once the foreign destination is removed by its owner, the kept journal and
    // backup restore the old version exactly.
    fs::remove_dir_all(&root).expect("the test's foreign destination");
    let notes = harness::recover_pending(&sandbox.data_root());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the restored old binary"),
        old_binary
    );
    assert!(!journal.exists());
    assert!(
        !fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
}

#[test]
fn a_kill_between_the_old_rename_and_its_journal_write_is_recovered() {
    let sandbox = Sandbox::new("kill-unjournaled");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let old_binary = fs::read(root.join("pi/pi")).expect("the old binary");
    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    command.env("GEWU_INSTALL_TEST_PAUSE", "after-old-rename-before-journal");
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let journal = harness.join(".install-journal");
    let ready = wait_until(
        || {
            fs::read_to_string(&journal)
                .map(|text| text.contains("\"staged\""))
                .unwrap_or(false)
                && !root.exists()
                && fs::read_dir(&harness)
                    .expect("the harness directory")
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
        },
        Duration::from_secs(15),
    );
    assert!(ready, "the installer reached the pre-journal pause");
    child.kill().expect("SIGKILL");
    let status = child.wait().expect("the installer is reaped");
    assert_eq!(status.signal(), Some(9));
    assert!(journal.exists());
    assert!(!stage_dirs(&harness).is_empty());

    let notes = harness::recover_pending(&sandbox.data_root());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        fs::read(root.join("pi/pi")).expect("the restored old binary"),
        old_binary
    );
    assert!(!journal.exists());
    assert!(stage_dirs(&harness).is_empty());
    assert!(
        !fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
}

#[test]
fn a_legacy_old_root_is_restored_when_the_version_root_is_missing() {
    let sandbox = Sandbox::new("legacy-old");
    let harness = sandbox.harness_dir("pi");
    let root = sandbox.install_root("pi", "1.4.0");
    let old = harness.join("1.4.old-4242");
    pi_old_install(&old, "1.4.0");
    assert!(!root.exists());
    let notes = harness::recover_pending(&sandbox.data_root());
    assert!(notes.is_empty(), "{notes:?}");
    assert!(root.join("pi/pi").is_file());
    assert!(!old.exists());
    assert!(harness::installed(&sandbox.data_root(), Program::Pi).is_some());
}

#[test]
fn a_hard_kill_after_the_new_root_is_published_keeps_the_verified_new_tree() {
    let sandbox = Sandbox::new("kill-published");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let archive = pi_archive(&sandbox.root);
    let digest = sha256_hex(&archive);
    let server = support::Server::body(archive, 64 * 1024, Duration::ZERO);
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    command.env("GEWU_INSTALL_TEST_PAUSE", "after-publish");
    let mut child = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the installer starts");
    let harness = sandbox.harness_dir("pi");
    let journal = harness.join(".install-journal");
    let ready = wait_until(
        || {
            fs::read_to_string(&journal)
                .map(|text| text.contains("\"published\""))
                .unwrap_or(false)
                && root.join("install.json").is_file()
        },
        Duration::from_secs(15),
    );
    assert!(ready, "the installer reached the pause after publication");
    child.kill().expect("SIGKILL");
    let status = child.wait().expect("the installer is reaped");
    assert_eq!(status.signal(), Some(9));
    assert!(journal.exists());
    assert!(root.join("install.json").is_file());
    assert!(
        fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );

    let notes = harness::recover_pending(&sandbox.data_root());
    assert!(notes.is_empty(), "{notes:?}");
    assert!(root.join("install.json").is_file());
    assert!(!journal.exists());
    assert!(
        fs::read_to_string(root.join("pi/pi"))
            .expect("the new binary")
            .contains("pi-fixture")
    );
    assert!(
        !fs::read_dir(&harness)
            .expect("the harness directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".old-"))
    );
}

#[test]
fn a_second_install_is_refused_and_a_repeat_after_success_is_ready_without_a_download() {
    let sandbox = Sandbox::new("parallel");
    let root = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&root, "1.3.9");
    let body = pi_archive(&sandbox.root);
    let digest = sha256_hex(&body);
    let server = support::Server::body(body, 1, Duration::from_millis(20));
    let mut command = sandbox.product();
    server.set_on(&mut command, &digest);
    let mut first = command
        .args(["--install-harness", "pi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the first installer starts");
    let harness = sandbox.harness_dir("pi");
    let ready = wait_until(
        || {
            stage_dirs(&harness).iter().any(|stage| {
                fs::metadata(stage.join("pi.tar.part"))
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
            })
        },
        Duration::from_secs(10),
    );
    assert!(ready, "the first download started");

    let second = run(&sandbox, "pi", &server, &digest);
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already being installed"), "{stderr}");

    let status = first.wait().expect("the first install ends");
    assert!(status.success());
    let repeat = sandbox
        .product()
        .args(["--install-harness", "pi"])
        .output()
        .expect("the repeat runs");
    assert!(repeat.status.success());
    assert!(
        String::from_utf8_lossy(&repeat.stdout).contains("is ready"),
        "{}",
        String::from_utf8_lossy(&repeat.stderr)
    );
}

#[test]
fn a_foreign_lock_is_not_removed() {
    let sandbox = Sandbox::new("foreign-lock");
    let harness = sandbox.harness_dir("pi");
    fs::create_dir_all(&harness).expect("the harness directory");
    let lock = harness.join(".install.lock");
    fs::write(&lock, b"a user's own file").expect("the foreign lock");
    let archive = pi_archive(&sandbox.root);
    let server = support::Server::body(archive.clone(), 64 * 1024, Duration::ZERO);
    let output = run(&sandbox, "pi", &server, &sha256_hex(&archive));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("did not write"), "{stderr}");
    assert_eq!(
        fs::read(&lock).expect("the foreign lock"),
        b"a user's own file"
    );
    assert!(stage_dirs(&harness).is_empty());
}

#[test]
fn blocked_dsh_scripts_are_diagnosed_and_the_old_install_is_kept() {
    let sandbox = Sandbox::new("dsh-blocked");
    let root = sandbox.install_root("dsh", "0.2.1-rc.2");
    dsh_old_install(&root, "0.2.0", 0o600);
    let old = fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint");

    let tree = sandbox.root.join("fake-tree");
    dsh_old_install(&tree, "0.2.1-rc.2", 0o600);
    // The fixture tree must carry only the package contents; its install.json is not
    // copied by npm.
    fs::remove_file(tree.join("install.json")).expect("the fixture record");
    write_executable(
        &sandbox.bin.join("npm"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf '%s\n' "${FAKE_NPM_VERSION:-11.17.0}"; exit 0; fi
if [ "$1" = "config" ]; then
  case "$3" in
    ignore-scripts) printf '%s\n' "${FAKE_IGNORE_SCRIPTS:-false}" ;;
    dangerously-allow-all-scripts) printf '%s\n' "${FAKE_DANGEROUS:-false}" ;;
    *) printf 'false\n' ;;
  esac
  exit 0
fi
prefix=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--prefix" ]; then prefix=$2; shift 2; continue; fi
  shift
done
cp -a "$FAKE_TREE/." "$prefix/"
exit 0
"#,
    );
    let mut command = sandbox.product();
    command
        .env("FAKE_TREE", &tree)
        .env("FAKE_IGNORE_SCRIPTS", "false")
        .args(["--install-harness", "dsh"]);
    let output = command.output().expect("the product runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("install script") || stderr.contains("terminal helper"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint"),
        old
    );
    assert!(stage_dirs(&sandbox.harness_dir("dsh")).is_empty());

    let mut command = sandbox.product();
    command
        .env("FAKE_TREE", &tree)
        .env("FAKE_IGNORE_SCRIPTS", "true")
        .args(["--install-harness", "dsh"]);
    let output = command.output().expect("the product runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("skips install scripts"), "{stderr}");
    assert_eq!(
        fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint"),
        old
    );
}

#[test]
fn unsupported_npm_and_dangerous_script_config_are_refused_before_effects() {
    let sandbox = Sandbox::new("dsh-npm-policy");
    let root = sandbox.install_root("dsh", "0.2.1-rc.2");
    dsh_old_install(&root, "0.2.0", 0);
    let old = fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint");
    let marker = sandbox.root.join("npm-ci-ran");
    write_executable(
        &sandbox.bin.join("npm"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf '%s
' "${FAKE_NPM_VERSION:-11.17.0}"; exit 0; fi
if [ "$1" = "config" ]; then
  case "$3" in
    ignore-scripts) printf '%s
' "${FAKE_IGNORE_SCRIPTS:-false}" ;;
    dangerously-allow-all-scripts) printf '%s
' "${FAKE_DANGEROUS:-false}" ;;
    *) printf 'false
' ;;
  esac
  exit 0
fi
: > "$NPM_CI_MARKER"
exit 0
"#,
    );
    for (version, dangerous, ignore, expected) in [
        ("11.15.0", "false", "false", "older than 11.16"),
        ("11.17.0", "true", "false", "dangerously-allow-all-scripts"),
        ("11.17.0", "false", "true", "skips install scripts"),
    ] {
        let _ = fs::remove_file(&marker);
        let mut command = sandbox.product();
        command
            .env("FAKE_NPM_VERSION", version)
            .env("FAKE_DANGEROUS", dangerous)
            .env("FAKE_IGNORE_SCRIPTS", ignore)
            .env("NPM_CI_MARKER", &marker)
            .args(["--install-harness", "dsh"]);
        let output = command.output().expect("the product runs");
        assert!(!output.status.success(), "{version}/{dangerous}/{ignore}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "{version}/{dangerous}/{ignore}: {stderr}"
        );
        assert!(
            !marker.exists(),
            "npm ci ran despite the preflight refusal: {version}/{dangerous}/{ignore}"
        );
        assert_eq!(
            fs::read(root.join("node_modules/.bin/dsh")).expect("the old entrypoint"),
            old
        );
        assert!(stage_dirs(&sandbox.harness_dir("dsh")).is_empty());
    }
}

#[test]
fn a_record_without_its_executable_or_helper_is_not_ready() {
    let sandbox = Sandbox::new("not-ready");
    let pi = sandbox.install_root("pi", "1.4.0");
    pi_old_install(&pi, "1.4.0");
    assert!(harness::installed(&sandbox.data_root(), Program::Pi).is_some());
    fs::set_permissions(
        pi.join("pi/pi"),
        <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .expect("the binary mode");
    assert!(harness::installed(&sandbox.data_root(), Program::Pi).is_none());

    let dsh = sandbox.install_root("dsh", "0.2.1-rc.2");
    dsh_old_install(&dsh, "0.2.1-rc.2", 0);
    assert!(harness::installed(&sandbox.data_root(), Program::Dsh).is_some());
    fs::remove_file(dsh.join(format!(
        "node_modules/node-pty/prebuilds/{}/pty.node",
        node_platform()
    )))
    .expect("the helper");
    assert!(harness::installed(&sandbox.data_root(), Program::Dsh).is_none());
}
