//! The confined shell really is confined: the product's seeds are not readable inside
//! it, while its workspace, its private temporary directory and the system tools are.
//!
//! The test drives the built binary's `--confined-shell` entry with the Landlock
//! method. On a kernel without Landlock the entry refuses with a sentence and the
//! test skips (the board says `runs are not confined on this computer` in that case).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-confinement-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .expect("the test root");
    root
}

fn write_private(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("a parent")).expect("the parent");
    fs::write(path, text).expect("the private file");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions).expect("mode 0600");
}

#[test]
fn the_confined_shell_cannot_read_the_products_own_files() {
    let root = scratch("seeds");
    let config = root.join("config");
    let state = root.join("state");
    let data = root.join("data");
    let workspace = config.join("agents/agent-1/workspace");
    let home = root.join("run-home");
    let temp = root.join("run-tmp");
    for dir in [&config, &state, &data, &workspace, &home, &temp] {
        fs::create_dir_all(dir).expect("a directory");
    }
    let credentials = config.join("credentials.toml");
    let password = data.join("forgejo/instance/secrets/admin-password");
    write_private(&credentials, "model_key = \"not-a-real-key\"\n");
    write_private(&password, "not-a-real-password");
    write_private(&data.join("forgejo/instance/app.ini"), "SECRET_KEY = x\n");
    // The same read outside the shell succeeds: the test is about the boundary.
    assert_eq!(
        fs::read_to_string(&credentials).expect("outside read"),
        "model_key = \"not-a-real-key\"\n"
    );

    let command = format!(
        "pwd; \
         cat {} 2>&1; echo credentials=$?; \
         cat {} 2>&1; echo password=$?; \
         cat {} 2>&1; echo appini=$?; \
         echo workspace-ok > out.txt && cat out.txt",
        credentials.display(),
        password.display(),
        data.join("forgejo/instance/app.ini").display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "landlock")
        .env("GEWU_CONFINE_COMMAND", &command)
        .env("GEWU_CONFINE_WORKSPACE", &workspace)
        .env(
            "GEWU_CONFINE_HIDE",
            format!(
                "{}:{}:{}",
                config.display(),
                state.display(),
                data.display()
            ),
        )
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .output()
        .expect("the confined shell runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && stderr.contains("runs are not confined on this computer") {
        eprintln!("Landlock is unavailable here; the board says runs are not confined");
        return;
    }
    assert!(
        output.status.success(),
        "the confined shell failed: {stderr}\n{stdout}"
    );
    assert_eq!(
        stdout.lines().next(),
        Some(workspace.display().to_string().as_str())
    );
    for what in ["credentials=", "password=", "appini="] {
        assert!(stdout.contains(what), "no {what} line: {stdout}");
    }
    assert_eq!(
        stdout.matches("Permission denied").count(),
        3,
        "a product file was readable inside the run: {stdout}"
    );
    assert!(stdout.contains("workspace-ok"), "{stdout}");
    // The workspace file really is in the agent's workspace.
    assert_eq!(
        fs::read_to_string(workspace.join("out.txt")).expect("the workspace write"),
        "workspace-ok\n"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_landlock_run_cannot_read_a_sibling_runs_askpass_file() {
    // B8's privacy property on the mechanism this host can exercise: the sibling
    // lives under the same engine temporary directory, is mode 0600 and the same
    // user, and is still outside this run's allow-list.
    let root = scratch("sibling-askpass");
    let workspace = root.join("workspace");
    let home = root.join("run-1/home");
    let temp = root.join("run-1/tmp");
    let sibling_temp = root.join("run-2/tmp");
    for dir in [&workspace, &home, &temp, &sibling_temp] {
        fs::create_dir_all(dir).expect("a directory");
    }
    let sibling = sibling_temp.join("askpass.toml");
    write_private(
        &sibling,
        "[forgejo_agent_tokens]\n\"agent-2\" = \"gw-fixture-sibling-token\"\n",
    );
    let command = format!("cat {} 2>&1; echo sibling=$?", sibling.display());
    let output = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "landlock")
        .env("GEWU_CONFINE_COMMAND", &command)
        .env("GEWU_CONFINE_WORKSPACE", &workspace)
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .output()
        .expect("the confined shell runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && stderr.contains("runs are not confined on this computer") {
        eprintln!("Landlock is unavailable here; the board says runs are not confined");
        return;
    }
    assert!(
        output.status.success(),
        "the confined shell failed: {stderr}\n{stdout}"
    );
    assert!(
        stdout.contains("Permission denied"),
        "a sibling run's askpass file was readable: {stdout}"
    );
    assert!(
        stdout.contains("sibling=1"),
        "the sibling read did not fail: {stdout}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_unconfined_shell_is_the_old_shell_and_the_board_says_so() {
    // The binary accepts the mode with `none` and runs the command unchanged; this is
    // the honest fallback the board's one sentence describes.
    let root = scratch("none");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let secret = root.join("secret");
    write_private(&secret, "readable when unconfined");
    let output = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "none")
        .env("GEWU_CONFINE_COMMAND", format!("cat {}", secret.display()))
        .env("GEWU_CONFINE_WORKSPACE", &workspace)
        .output()
        .expect("the shell runs");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("readable when unconfined"),
        "the fallback changed the command's view"
    );
    let _ = fs::remove_dir_all(&root);
}

/// B10: a host whose kernel does not implement Landlock (`ENOSYS`) or has it
/// disabled (`EOPNOTSUPP`) must be called unconfined, show the sixth warning, and
/// refuse the shell instead of running the command open.
///
/// The runner sets `GEWU_B10_SHIM=1` and `LD_PRELOAD` to the reviewer's shim. Without
/// them the host is the control and the test skips; the existing tests above prove
/// the confined path on this kernel.
#[test]
fn a_host_without_landlock_is_unconfined_and_refuses_the_shell() {
    if std::env::var_os("GEWU_B10_SHIM").is_none() {
        eprintln!("no Landlock-absence shim is active; the host control is covered above");
        return;
    }
    let root = scratch("b10-no-landlock");
    let config = root.join("config");
    let state = root.join("state");
    let data = root.join("data");
    let workspace = config.join("agents/agent-1/workspace");
    for dir in [&config, &state, &data, &workspace] {
        fs::create_dir_all(dir).expect("a directory");
    }
    // `detect` is the same function the engine calls when it opens.
    let method = gewu_multiverse::confinement::detect(
        &config,
        &state,
        &data,
        std::slice::from_ref(&workspace),
    );
    assert_eq!(
        method,
        gewu_multiverse::confinement::Method::None,
        "a kernel without Landlock was called confined"
    );
    let layout = gewu_multiverse::paths::Layout::rooted_at(config.clone()).expect("the layout");
    let mut options =
        gewu_multiverse::engine::EngineOptions::new(layout, &data).expect("the engine options");
    assert_eq!(
        options.confinement,
        gewu_multiverse::confinement::Method::None,
        "the engine's detection still claims a boundary"
    );
    options.auto_start_forgejo = false;
    let mut engine =
        gewu_multiverse::engine::open_with(options).expect("the engine opens without Forgejo");
    let board = gewu_multiverse::engine_api::EngineApi::board(&mut engine);
    assert!(!board.confined, "the board pretends the run is confined");
    assert_eq!(
        board.terms.len(),
        gewu_multiverse::engine::TERMS.len() + 1,
        "the sixth warning is missing: {:?}",
        board.terms
    );
    assert_eq!(
        board.terms.last().map(String::as_str),
        Some("Runs are not confined on this computer."),
        "the sixth warning has the wrong words: {:?}",
        board.terms
    );
    drop(engine);

    let secret = root.join("secret");
    write_private(&secret, "TOPSECRET");
    let home = root.join("home");
    let temp = root.join("tmp");
    for dir in [&home, &temp] {
        fs::create_dir_all(dir).expect("a directory");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "landlock")
        .env("GEWU_CONFINE_COMMAND", format!("cat {}", secret.display()))
        .env("GEWU_CONFINE_WORKSPACE", &workspace)
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .output()
        .expect("the confined shell runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the shell ran on a host without Landlock: {stdout}"
    );
    assert!(
        stderr.contains("runs are not confined on this computer"),
        "the refusal does not carry the sentence: {stderr}"
    );
    assert!(
        !stdout.contains("TOPSECRET"),
        "the secret was printed instead of refused: {stdout}"
    );
    let _ = fs::remove_dir_all(&root);
}
