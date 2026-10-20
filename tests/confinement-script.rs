//! The complete `--confined-shell` namespace paths, executed on this host with fake
//! `unshare`/`mount` and fake `bwrap` programs.
//!
//! This host has no user namespaces and no `bwrap`, so the real mounts cannot run.
//! The product's own binary is still executed end to end: a fake `unshare` runs the
//! script it was handed, and a fake `mount` records every call, refuses a missing
//! mount point exactly as mount(8) does, and changes nothing. A fake `bwrap` parses
//! the complete argument list the way bubblewrap does, refuses a non-numeric
//! `--file` (B5-R-R), creates a bind destination only where bubblewrap's
//! `ensure_node_at` would, and then runs the command. That exercises the whole
//! invocation the product builds (B5-R), including the order that keeps this run's
//! own root private from its siblings (B8).
//!
//! The tests also copy the product binary before running it, so a script or argument
//! list that truncates a binary that was not hidden fails the hash check instead of
//! breaking the build's binary (B5-R's second defect).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-unshare-probe-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&root).expect("the test root");
    root
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("the executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("the mode");
}

const FAKE_MOUNT: &str = "#!/bin/sh\n\
    printf '%s\\n' \"$*\" >> \"$GEWU_FAKE_MOUNT_LOG\"\n\
    case \"$1\" in\n\
      --make-rprivate) exit 0 ;;\n\
      --bind)\n\
        [ -e \"$2\" ] || { echo \"missing source $2\" >&2; exit 32; }\n\
        [ -e \"$3\" ] || { echo \"missing target $3\" >&2; exit 33; }\n\
        ;;\n\
      -t)\n\
        [ -e \"$4\" ] || { echo \"missing tmpfs target $4\" >&2; exit 34; }\n\
        ;;\n\
    esac\n\
    exit 0\n";

#[test]
fn the_product_builds_a_complete_unshare_script_that_can_start() {
    let root = scratch("complete");
    let fake = root.join("fake-bin");
    fs::create_dir_all(&fake).expect("the fake bin");
    write_executable(&fake.join("mount"), FAKE_MOUNT);
    write_executable(&fake.join("umount"), "#!/bin/sh\nexit 0\n");
    // The fake `unshare` runs the script the product built.
    write_executable(
        &fake.join("unshare"),
        "#!/bin/sh\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",
    );

    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("the bin directory");
    let product = bin.join("gewu-multiverse");
    fs::copy(env!("CARGO_BIN_EXE_gewu-multiverse"), &product).expect("the product copy");
    fs::set_permissions(&product, fs::Permissions::from_mode(0o700)).expect("the mode");
    let before = fs::read(&product).expect("the copied binary");

    let config = root.join("config");
    let state = root.join("state");
    let data = root.join("data");
    let workspace = config.join("agents/agent-1/workspace");
    let home = root.join("home");
    let engine_temp = root.join("engine-tmp");
    let run_root = engine_temp.join("gewu-run-1");
    let sibling = engine_temp.join("gewu-run-2");
    let stage = root.join("stage");
    let log = root.join("mounts.log");
    for path in [
        &config,
        &state,
        &data,
        &workspace,
        &home,
        &engine_temp,
        &run_root,
        &sibling,
        &stage,
    ] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    fs::create_dir_all(sibling.join("tmp")).expect("the sibling tmp");
    fs::write(
        sibling.join("tmp/askpass.toml"),
        "[forgejo_agent_tokens]\n\"agent-2\" = \"sibling-token\"\n",
    )
    .expect("the sibling file");

    let output = Command::new(&product)
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "unshare")
        .env("GEWU_CONFINE_COMMAND", "true")
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
        .env("GEWU_CONFINE_ROOT", &run_root)
        .env("GEWU_CONFINE_STAGE", &stage)
        .env("TMPDIR", &engine_temp)
        .env("HOME", &home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GEWU_FAKE_MOUNT_LOG", &log)
        .output()
        .expect("the product runs");

    assert!(
        output.status.success(),
        "the complete unshare script could not start: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).expect("the mount log");
    assert!(calls.contains("-t tmpfs tmpfs /tmp"), "{calls}");
    assert!(
        calls.contains(&format!(
            "--bind {} {}",
            stage.join("root").display(),
            run_root.display()
        )),
        "{calls}"
    );
    assert!(
        !calls.contains(&sibling.display().to_string()),
        "a sibling run was bound into the namespace: {calls}"
    );
    // The unconditional truncation of B5-R would have changed this copy even when
    // the binary was not inside a hidden root.
    assert_eq!(
        fs::read(&product).expect("the copied binary is still there"),
        before,
        "the script truncated a product binary that was not hidden"
    );
    let _ = fs::remove_dir_all(&root);
}

const FAKE_BWRAP: &str = "#!/bin/sh\n\
    set -u\n\
    log=\"${GEWU_FAKE_BWRAP_LOG:?}\"\n\
    printf '%s\\n' \"$*\" >> \"$log\"\n\
    chdir=\n\
    while [ \"$#\" -gt 0 ]; do\n\
      case \"$1\" in\n\
        --dev-bind|--bind|--ro-bind)\n\
          [ -e \"$2\" ] || { echo \"bwrap: missing bind source $2\" >&2; exit 31; }\n\
          if [ -L \"$3\" ]; then echo \"bwrap: Can't mount on symlink destination $3\" >&2; exit 42; fi\n\
          if [ ! -e \"$3\" ]; then\n\
            parent=$(dirname \"$3\")\n\
            [ -d \"$parent\" ] || { echo \"bwrap: missing parent $parent of $3\" >&2; exit 33; }\n\
            if [ -d \"$2\" ]; then mkdir \"$3\" || exit 34; else : > \"$3\" || exit 34; fi\n\
          fi\n\
          shift 3 ;;\n\
        --tmpfs)\n\
          if [ -L \"$2\" ]; then echo \"bwrap: Can't mount on symlink destination $2\" >&2; exit 42; fi\n\
          [ -e \"$2\" ] || { echo \"bwrap: missing tmpfs target $2\" >&2; exit 35; }\n\
          shift 2 ;;\n\
        --dir)\n\
          mkdir -p \"$2\" || { echo \"bwrap: cannot make $2\" >&2; exit 36; }\n\
          shift 2 ;;\n\
        --chdir)\n\
          chdir=$2\n\
          shift 2 ;;\n\
        --file)\n\
          case \"$2\" in\n\
            ''|*[!0-9]*) echo \"bwrap: invalid file descriptor '$2' for --file\" >&2; exit 42 ;;\n\
          esac\n\
          shift 3 ;;\n\
        --)\n\
          shift\n\
          break ;;\n\
        -*)\n\
          echo \"bwrap: unsupported option $1\" >&2\n\
          exit 43 ;;\n\
        *)\n\
          break ;;\n\
      esac\n\
    done\n\
    [ \"$#\" -gt 0 ] || { echo 'bwrap: no command' >&2; exit 44; }\n\
    if [ -n \"$chdir\" ]; then cd \"$chdir\" || exit 45; fi\n\
    exec \"$@\"\n";

/// A directory outside `/tmp` and outside every hide, beside the test binary.
///
/// N16: a visible-binary test must not depend on the cargo target living outside
/// `/tmp`. When this host's target is under `/tmp`, `None` says to use the build
/// binary and compute the expected restore branch from its path instead.
fn outside_tmp_dir(tag: &str) -> Option<PathBuf> {
    let parent = std::env::current_exe().ok()?.parent()?.to_path_buf();
    if parent.starts_with("/tmp") || parent.starts_with(std::env::temp_dir()) {
        return None;
    }
    let dir = parent.join(format!(
        "gewu-confinement-test-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The bwrap path: a copy of the product executes the complete argument list the
/// product builds against a fake bwrap that validates it the way bubblewrap does.
/// `hidden` puts the product binary under the configuration root, so the restore
/// branch runs; otherwise the binary is placed where it is visible and must not be
/// replaced. The returned `restore` says which branch the path itself implies, so
/// the test never assumes the cargo target's location (N16).
#[allow(clippy::type_complexity)]
fn run_fake_bwrap(
    tag: &str,
    hidden: bool,
) -> (
    PathBuf,
    PathBuf,
    PathBuf,
    std::process::Output,
    Vec<u8>,
    PathBuf,
    bool,
) {
    let root = scratch(tag);
    let fake = root.join("fake-bin");
    fs::create_dir_all(&fake).expect("the fake bin");
    write_executable(&fake.join("bwrap"), FAKE_BWRAP);

    let config = root.join("config");
    let state = root.join("state");
    let data = root.join("data");
    let workspace = config.join("agents/agent-1/workspace");
    let home = root.join("home");
    let engine_temp = root.join("engine-tmp");
    let run_root = engine_temp.join("gewu-run-1");
    let sibling = engine_temp.join("gewu-run-2");
    let stage = root.join("stage");
    for path in [
        &config,
        &state,
        &data,
        &workspace,
        &home,
        &engine_temp,
        &run_root,
        &sibling,
        &stage,
    ] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    fs::create_dir_all(stage.join("workspace")).expect("the stage workspace");
    fs::create_dir_all(stage.join("root")).expect("the stage root");
    fs::write(stage.join("askpass-exe"), b"#!/bin/sh\nexit 0\n").expect("the staged binary");
    fs::create_dir_all(sibling.join("tmp")).expect("the sibling tmp");
    fs::write(
        sibling.join("tmp/askpass.toml"),
        "[forgejo_agent_tokens]\n\"agent-2\" = \"sibling-token\"\n",
    )
    .expect("the sibling file");

    // The product copy under the hidden configuration root is the process's own
    // executable, so `--confined-shell` restores exactly the file the person
    // installed. The visible case is either a copy beside the test binary (outside
    // `/tmp` and every hide) or, when the cargo target is under `/tmp`, the build
    // binary whose expected branch is computed from its own path (N16).
    let product = if hidden {
        let product = config.join("bin/gewu-multiverse");
        fs::create_dir_all(product.parent().expect("a parent")).expect("the product directory");
        fs::copy(env!("CARGO_BIN_EXE_gewu-multiverse"), &product).expect("the product copy");
        fs::set_permissions(&product, fs::Permissions::from_mode(0o700)).expect("the mode");
        product
    } else if let Some(dir) = outside_tmp_dir("visible") {
        let product = dir.join("gewu-multiverse");
        fs::copy(env!("CARGO_BIN_EXE_gewu-multiverse"), &product).expect("the product copy");
        fs::set_permissions(&product, fs::Permissions::from_mode(0o700)).expect("the mode");
        product
    } else {
        PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"))
    };
    let restore = product.starts_with("/tmp")
        || product.starts_with(&engine_temp)
        || [&config, &state, &data, &home]
            .iter()
            .any(|root| product.starts_with(root));
    let before = fs::read(&product).expect("the product binary");

    let log = root.join("bwrap.log");
    let output = Command::new(&product)
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "bwrap")
        .env("GEWU_CONFINE_COMMAND", "printf 'bwrap-ran\\n' > ran.txt")
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
        .env("GEWU_CONFINE_ROOT", &run_root)
        .env("GEWU_CONFINE_STAGE", &stage)
        .env("TMPDIR", &engine_temp)
        .env("HOME", &home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GEWU_FAKE_BWRAP_LOG", &log)
        .output()
        .expect("the product runs");
    (root, workspace, log, output, before, product, restore)
}

#[test]
fn the_complete_bwrap_invocation_runs_and_restores_a_hidden_binary() {
    let (root, workspace, log, output, before, _product, restore) =
        run_fake_bwrap("bwrap-complete-hidden", true);
    assert!(restore, "the hidden case must need the restore branch");
    assert!(
        output.status.success(),
        "the complete bwrap argument list could not start: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).expect("the bwrap log");
    // The private /tmp comes before the own-root/workspace rebinds.
    let private = calls
        .find("--tmpfs /tmp")
        .unwrap_or_else(|| panic!("the private /tmp is missing: {calls}"));
    let own_root = format!(
        "--bind {} {}",
        root.join("stage/root").display(),
        root.join("engine-tmp/gewu-run-1").display()
    );
    let root_back = calls
        .find(&own_root)
        .unwrap_or_else(|| panic!("this run's root was not bound back: {calls}"));
    assert!(
        private < root_back,
        "this run's root is bound before /tmp is private: {calls}"
    );
    assert!(
        calls.contains(&format!(
            "--bind {} {}",
            root.join("stage/root").display(),
            root.join("engine-tmp/gewu-run-1").display()
        )),
        "this run's root was not bound back: {calls}"
    );
    assert!(
        calls.contains(&format!(
            "--bind {} {}",
            root.join("stage/workspace").display(),
            workspace.display()
        )),
        "this run's workspace was not bound back: {calls}"
    );
    assert!(
        calls.contains(&format!(
            "--ro-bind {} {}",
            root.join("stage/askpass-exe").display(),
            root.join("config/bin/gewu-multiverse").display()
        )),
        "the hidden product binary was not restored: {calls}"
    );
    // B5-R-R: the old invocation passed a path to `--file`, whose first argument is
    // a descriptor number. The fake rejects it with rc 42; the fixed list has none.
    assert!(
        !calls.contains("--file"),
        "a --file argument was passed: {calls}"
    );
    // A sibling run is never named.
    assert!(
        !calls.contains(&root.join("engine-tmp/gewu-run-2").display().to_string()),
        "a sibling run was bound into the namespace: {calls}"
    );
    // The command the product built actually ran in the workspace.
    assert_eq!(
        fs::read_to_string(workspace.join("ran.txt")).expect("the command's file"),
        "bwrap-ran\n"
    );
    // A visible product binary is never truncated.
    assert_eq!(
        before,
        fs::read(root.join("config/bin/gewu-multiverse")).expect("the binary"),
        "the bwrap path truncated the product binary"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_complete_bwrap_invocation_replaces_only_a_binary_the_namespace_hides() {
    // N16: the expected restore branch is computed from the binary's own path, so
    // this passes whether the cargo target lives under `/tmp` or not.
    let (root, workspace, log, output, before, product, restore) =
        run_fake_bwrap("bwrap-complete-visible", false);
    assert!(
        output.status.success(),
        "the bwrap path with a visible binary failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).expect("the bwrap log");
    assert!(!calls.contains("--file"), "{calls}");
    let replacement = format!(
        "--ro-bind {} {}",
        root.join("stage/askpass-exe").display(),
        product.display()
    );
    assert_eq!(
        calls.contains(&replacement),
        restore,
        "the restore branch did not follow the binary path {} (expected restore={restore}): {calls}",
        product.display()
    );
    assert_eq!(
        before,
        fs::read(&product).expect("the binary"),
        "the product binary changed"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("ran.txt")).expect("the command's file"),
        "bwrap-ran\n"
    );
    if product.starts_with(std::env::temp_dir()) || product.starts_with("/tmp") {
        // The build binary belongs to cargo; never remove it.
    } else if product.starts_with(&root) {
        // Under the fixture root, removed below.
    } else {
        let _ = fs::remove_file(&product);
    }
    let _ = fs::remove_dir_all(&root);
}

/// Run the product's `--confined-shell` for one bwrap invocation. The fake bwrap
/// rejects a mount on a symlink and logs the complete argument list.
fn run_bwrap_product(
    root: &Path,
    hide: &[&Path],
    home: &Path,
    workspace: &Path,
) -> (std::process::Output, String) {
    let fake = root.join("fake-bin");
    fs::create_dir_all(&fake).expect("the fake bin");
    write_executable(&fake.join("bwrap"), FAKE_BWRAP);
    let engine_temp = root.join("engine-tmp");
    let run_root = engine_temp.join("gewu-run-1");
    let stage = root.join("stage");
    for path in [&engine_temp, &run_root, &stage] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    fs::create_dir_all(stage.join("workspace")).expect("the stage workspace");
    fs::create_dir_all(stage.join("root")).expect("the stage root");
    fs::write(stage.join("askpass-exe"), b"#!/bin/sh\nexit 0\n").expect("the staged binary");
    let log = root.join("bwrap.log");
    let output = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--confined-shell")
        .env("GEWU_CONFINE_METHOD", "bwrap")
        .env("GEWU_CONFINE_COMMAND", "printf 'bwrap-ran\\n' > ran.txt")
        .env("GEWU_CONFINE_WORKSPACE", workspace)
        .env(
            "GEWU_CONFINE_HIDE",
            hide.iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(":"),
        )
        .env("GEWU_CONFINE_ROOT", &run_root)
        .env("GEWU_CONFINE_STAGE", &stage)
        .env("TMPDIR", &engine_temp)
        .env("HOME", home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GEWU_FAKE_BWRAP_LOG", &log)
        .output()
        .expect("the product runs");
    (output, fs::read_to_string(&log).unwrap_or_default())
}

#[test]
fn the_bwrap_path_resolves_a_hidden_symlink_under_the_engine_tmpdir() {
    // B5-R-R-R: the state root's name is under the engine TMPDIR (so the private
    // `/tmp` tmpfs would skip it), but its target is outside both. Without
    // resolution bubblewrap receives `--tmpfs <symlink>` and refuses the whole
    // invocation; the target would also stay visible.
    let Some(outside) = outside_tmp_dir("symlink-target") else {
        eprintln!("no writable place outside /tmp for this test's target; skipped");
        return;
    };
    let root = scratch("bwrap-symlink-hide");
    let config = root.join("config");
    let data = root.join("data");
    let home = root.join("home");
    let workspace = root.join("workspace");
    for path in [&config, &data, &home, &workspace] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    let target = outside.join("real-state");
    fs::create_dir_all(&target).expect("the state target");
    let state_link = root.join("state-link");
    std::os::unix::fs::symlink(&target, &state_link).expect("the state symlink");
    let (output, calls) =
        run_bwrap_product(&root, &[&config, &state_link, &data], &home, &workspace);
    assert!(
        output.status.success(),
        "the bwrap path refused a resolvable symlinked hide: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        calls.contains(&format!("--tmpfs {}", target.display())),
        "the symlink's target was not hidden: {calls}"
    );
    assert!(
        !calls.contains(&format!("--tmpfs {}", state_link.display())),
        "a symlink was handed to bwrap as a mount destination: {calls}"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("ran.txt")).expect("the command's file"),
        "bwrap-ran\n"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

#[test]
fn the_bwrap_path_resolves_a_symlinked_config_dir_under_the_home() {
    // B5-R-R-R: `~/.config/gewu` is a link into `~/dotfiles`. The home and the
    // configuration root are both hidden; the link itself must be resolved before it
    // reaches bubblewrap, which refuses to mount on a symlink destination.
    let Some(outside) = outside_tmp_dir("home-target") else {
        eprintln!("no writable place outside /tmp for this test's home; skipped");
        return;
    };
    let root = scratch("bwrap-symlink-home");
    let home = outside.join("home");
    let target = outside.join("dotfiles/gewu-config");
    let data = root.join("data");
    let workspace = root.join("workspace");
    for path in [&target, &data, &workspace] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    fs::create_dir_all(home.join(".config")).expect("the home's config directory");
    let config_link = home.join(".config/gewu");
    std::os::unix::fs::symlink(&target, &config_link).expect("the configuration symlink");
    let state = root.join("state");
    fs::create_dir_all(&state).expect("the state root");
    let (output, calls) = run_bwrap_product(
        &root,
        &[&config_link, &state, &data, &home],
        &home,
        &workspace,
    );
    assert!(
        output.status.success(),
        "the bwrap path refused a symlinked configuration root: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        calls.contains(&format!("--tmpfs {}", target.display())),
        "the configuration link's target was not hidden: {calls}"
    );
    assert!(
        !calls.contains(&format!("--tmpfs {}", config_link.display())),
        "a symlink was handed to bwrap as a mount destination: {calls}"
    );
    assert!(
        calls.contains(&format!("--tmpfs {}", home.display())),
        "the home itself was not hidden: {calls}"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

/// Run the selection test's child process. `fake_bwrap` puts a working fake
/// `bwrap` first on `PATH`; without it `PATH` holds only the empty fake directory,
/// so no namespace can be found on any host (N18).
fn run_selection_child(
    opt_in: Option<&str>,
    fake_bwrap: bool,
) -> (std::process::Output, PathBuf, bool) {
    let root = scratch(&format!("selection-{}", opt_in.unwrap_or("default")));
    let fake = root.join("fake-bin");
    fs::create_dir_all(&fake).expect("the fake bin");
    if fake_bwrap {
        write_executable(&fake.join("bwrap"), FAKE_BWRAP);
    }
    // N17: the selection tests need no runtime directory outside `/tmp`; when the
    // cargo target is under `/tmp`, leave `XDG_RUNTIME_DIR` unset and let the
    // staging probe use `/dev/shm` or `/var/tmp` as it does in production.
    let xdg = outside_tmp_dir("runtime");
    let home = root.join("home");
    let temp = root.join("tmp");
    for path in [&home, &temp] {
        fs::create_dir_all(path).expect("a fixture directory");
    }
    let log = root.join("bwrap.log");
    let mut command = Command::new(std::env::current_exe().expect("the test binary"));
    command
        .args(["--exact", "selection_child", "--ignored", "--nocapture"])
        .env("GEWU_SELECTION_CHILD", "1")
        .env("GEWU_SELECTION_WORK", &root)
        .env("GEWU_FAKE_BWRAP_LOG", &log)
        .env("HOME", &home)
        .env("TMPDIR", &temp)
        .env(
            "PATH",
            if fake_bwrap {
                format!(
                    "{}:{}",
                    fake.display(),
                    std::env::var("PATH").unwrap_or_default()
                )
            } else {
                fake.display().to_string()
            },
        )
        .env_remove("GEWU_CONFINEMENT");
    if let Some(xdg) = xdg.as_deref() {
        command.env("XDG_RUNTIME_DIR", xdg);
    }
    if let Some(kind) = opt_in {
        command.env("GEWU_CONFINEMENT", kind).env(
            "GEWU_SELECTION_EXPECT",
            if fake_bwrap { kind } else { "none" },
        );
    }
    let staged = xdg.is_some();
    let output = command.output().expect("the selection child runs");
    if let Some(xdg) = xdg {
        let _ = fs::remove_dir_all(xdg);
    }
    (output, log, staged)
}

#[test]
#[ignore = "child process of the selection tests; never run directly"]
fn selection_child() {
    let work = PathBuf::from(std::env::var("GEWU_SELECTION_WORK").expect("a work directory"));
    let config = work.join("config");
    let state = work.join("state");
    let data = work.join("data");
    let workspace = config.join("agents/agent-1/workspace");
    for path in [&config, &state, &data, &workspace] {
        fs::create_dir_all(path).expect("a child fixture directory");
    }
    let method = gewu_multiverse::confinement::detect(
        &config,
        &state,
        &data,
        std::slice::from_ref(&workspace),
    );
    match std::env::var("GEWU_SELECTION_EXPECT").ok().as_deref() {
        Some("bwrap") => {
            assert_eq!(
                method,
                gewu_multiverse::confinement::Method::Bwrap,
                "the opt-in did not select the working bwrap"
            );
            let log = std::env::var("GEWU_FAKE_BWRAP_LOG").expect("the fake bwrap log");
            assert!(
                fs::read_to_string(log).is_ok_and(|text| !text.is_empty()),
                "the fake bwrap on PATH was never used for the opt-in"
            );
        }
        // N18: the person asked for a namespace that cannot be honoured. The answer
        // is none, with the sixth sentence on the board; substituting Landlock would
        // be a different boundary than the one requested.
        Some("none") => assert_eq!(
            method,
            gewu_multiverse::confinement::Method::None,
            "an opt-in that cannot be honoured was replaced by another mechanism"
        ),
        _ => assert!(
            method == gewu_multiverse::confinement::Method::Landlock
                || method == gewu_multiverse::confinement::Method::None,
            "the default selection chose a namespace without the opt-in: {method:?}"
        ),
    }
}

#[test]
fn the_default_selection_never_chooses_a_namespace_even_with_a_working_fake_bwrap() {
    let (output, _log, _staged) = run_selection_child(None, true);
    assert!(
        output.status.success(),
        "the default-selection child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_opt_in_selects_the_working_namespace() {
    let (output, _log, staged) = run_selection_child(Some("bwrap"), true);
    if !staged {
        // N17: a cargo target under `/tmp` leaves no runtime directory outside the
        // temporary tree the namespace must replace, so the opt-in cannot be staged
        // on this host layout. The same skip the symlink tests use; the production
        // path picks `/dev/shm` or `/var/tmp` when they are usable.
        eprintln!("no staging directory outside /tmp for the namespace opt-in; skipped");
        return;
    }
    assert!(
        output.status.success(),
        "the opt-in child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_opt_in_that_cannot_be_honoured_gives_none_not_landlock() {
    // N18: no `bwrap` exists anywhere on the child's `PATH`, but Landlock does. The
    // person asked for bwrap, so the honest answer is none and the board carries the
    // sixth sentence; silently giving Landlock would be a different mechanism.
    let (output, _log, _staged) = run_selection_child(Some("bwrap"), false);
    assert!(
        output.status.success(),
        "an unhonourable opt-in fell back to another mechanism: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_fake_bwrap_models_symlink_rejection_and_directory_creation() {
    // The fake is part of the proof for B5-R-R-R, so its two bubblewrap rules are
    // themselves checked: a mount on a symlink is refused, and a missing bind
    // destination is created with the source's node type.
    let root = scratch("fake-bwrap-contract");
    let fake = root.join("bwrap");
    write_executable(&fake, FAKE_BWRAP);
    let log = root.join("log");
    let source = root.join("source-dir");
    fs::create_dir_all(&source).expect("the source directory");
    let destination = root.join("missing-dir");
    let status = Command::new(&fake)
        .args(["--bind"])
        .arg(&source)
        .arg(&destination)
        .arg("/bin/true")
        .env("GEWU_FAKE_BWRAP_LOG", &log)
        .status()
        .expect("the fake runs");
    assert!(status.success(), "the fake refused a plain directory bind");
    assert!(
        destination.is_dir(),
        "the fake created a file for a directory source"
    );
    let link = root.join("link");
    std::os::unix::fs::symlink(&source, &link).expect("the symlink");
    let status = Command::new(&fake)
        .args(["--tmpfs"])
        .arg(&link)
        .arg("/bin/true")
        .env("GEWU_FAKE_BWRAP_LOG", &log)
        .status()
        .expect("the fake runs");
    assert_eq!(
        status.code(),
        Some(42),
        "the fake accepted a mount on a symlink destination"
    );
    let _ = fs::remove_dir_all(&root);
}
