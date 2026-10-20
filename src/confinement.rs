//! Keeping an agent's shell out of the product's own files.
//!
//! A run's shell is a process the person's model can ask to do anything. It must
//! work in its own workspace, with `/tmp`, the network and the tools it needs — and
//! it must not be able to read the product's configuration root, its state root, the
//! bundled Forgejo's data root, any credentials file, or another run's login file.
//! This module chooses the strongest mechanism this computer offers and applies it
//! to the shell only. The engine and the run's own bookkeeping stay outside.
//!
//! The **automatic** mechanism is Landlock, the unprivileged kernel sandbox: the
//! shell may read only the workspace, its own private temporary directory (where its
//! per-run askpass file lives), the system's tools and `/dev`; every other path is
//! denied. Landlock needs no user namespace and is the one mechanism the product
//! chooses without being asked. It is claimed only after a child process proved that
//! a `HardRequirement` ruleset is fully enforced and denies a read outside the grant:
//! a kernel without Landlock, or one where the ruleset is not fully enforced, gives
//! [`Method::None`] instead of a dummy ruleset (B10).
//!
//! Two namespace mechanisms are kept for hosts that can run them, and are chosen
//! **only when the person opts in** with `GEWU_CONFINEMENT=bwrap` or
//! `GEWU_CONFINEMENT=unshare`, because their complete argument lists are tested
//! against bubblewrap's and mount(8)'s contracts but they have not yet been proven on
//! a real user-namespace host:
//!
//! 1. `bwrap` (bubblewrap), a user and mount namespace with the protected
//!    directories replaced by empty tmpfs mounts and the workspace bound back into
//!    place;
//! 2. `unshare --user --map-root-user --mount`, the same picture built from shell
//!    `mount` calls.
//!
//! Both namespace mechanisms also replace `/tmp` and the engine's own temporary
//! directory with a private tmpfs, then bind back only **this** run's directory and
//! the workspace. A sibling run's `tmp/askpass.toml`, mode 0600 and the same user,
//! is therefore not visible: `0600` stops another user, not the same one. The
//! staging copies the mounts need are made in `/dev/shm` (or `$XDG_RUNTIME_DIR`, or
//! `/var/tmp`), mode 0700, and removed when the run ends. Every path a mount is
//! placed on is resolved with `realpath` first: bubblewrap refuses to mount on a
//! symlink destination, and a symlink under `/tmp` whose target is elsewhere would
//! otherwise survive the private `/tmp` with its target visible.
//!
//! Landlock cannot subtract a rule from a tree it was told to allow. If the
//! configuration, state or Forgejo data root lies inside an allow-listed tree
//! (`/usr`, `/bin`, `/sbin`, `/lib*`, `/etc`, `/opt`, `/proc`, `/sys`, `/run`,
//! `/dev`, the product binary's directory or a workspace), Landlock is not chosen:
//! the method is [`Method::None`] and the board and the first-run warnings say `runs
//! are not confined on this computer` instead of pretending. The namespace
//! mechanisms are never substituted automatically.
//!
//! The shell never inherits the person's environment: [`crate::agent_run`] clears it
//! and passes a short allow-list (`PATH`, `LANG`/`LC_*`, `TERM`, `TZ`) plus the
//! product's own run variables. Git authenticates through the product's
//! `--git-askpass` helper with a per-run file that holds only that run's own Forgejo
//! login and token; the product's `credentials.toml` — the model key and every other
//! agent's login — is outside the sandbox.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use landlock::{
    ABI, Access, AccessFs, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus,
};

/// The environment variable that names the chosen mechanism to `--confined-shell`.
pub const ENV_METHOD: &str = "GEWU_CONFINE_METHOD";
/// The shell command the confined shell runs.
pub const ENV_COMMAND: &str = "GEWU_CONFINE_COMMAND";
/// The workspace the shell is rooted in.
pub const ENV_WORKSPACE: &str = "GEWU_CONFINE_WORKSPACE";
/// The colon-separated paths that must not be visible.
pub const ENV_HIDE: &str = "GEWU_CONFINE_HIDE";
/// This run's private root (its empty `home` and its own `tmp`).
pub const ENV_ROOT: &str = "GEWU_CONFINE_ROOT";
/// The writable staging directory, outside every hidden tree, where the namespace
/// mounts preserve this run's root, workspace and askpass binary across the hides.
pub const ENV_STAGE: &str = "GEWU_CONFINE_STAGE";
/// The person's opt-in to a namespace mechanism, `bwrap` or `unshare`. Landlock is
/// the automatic mechanism; a namespace is never selected without this word.
pub const ENV_REQUESTED: &str = "GEWU_CONFINEMENT";

/// The bytes the enforcement probe must *not* be able to read. The parent writes
/// them into the probe root; the confined child reads the allowed control file and
/// must be denied this one. The value is not a secret.
const PROBE_SECRET: &[u8] = b"TOPSECRET";

/// The trees Landlock may read, on top of the run's own workspace, `TMPDIR` and
/// `HOME`. The names are the same ones [`exec_landlock`] grants.
const LANDLOCK_READ_TREES: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/proc", "/sys", "/run",
];

/// The subtrees Landlock may read and write on top of the run's own directories.
const LANDLOCK_WRITE_TREES: &[&str] = &["/dev"];

/// How long a capability probe may take. A host that hangs on `unshare` is not a
/// host that can confine a run; the board must still open.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The mechanism that confines a run's shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    /// A user and mount namespace through bubblewrap.
    Bwrap,
    /// A user and mount namespace through util-linux `unshare`.
    Unshare,
    /// The Landlock kernel sandbox.
    Landlock,
    /// This computer offers none of them.
    #[default]
    None,
}

impl Method {
    /// The word written into the run plan.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Unshare => "unshare",
            Self::Landlock => "landlock",
            Self::None => "none",
        }
    }

    /// Read a plan's word back. An unknown word is no confinement, never a pretend.
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "bwrap" => Self::Bwrap,
            "unshare" => Self::Unshare,
            "landlock" => Self::Landlock,
            _ => Self::None,
        }
    }

    /// Whether this mechanism actually hides the product's files.
    pub fn confined(self) -> bool {
        self != Self::None
    }

    /// Whether this mechanism builds a mount namespace.
    pub fn namespace(self) -> bool {
        matches!(self, Self::Bwrap | Self::Unshare)
    }
}

/// The namespace mechanism the person asked for, if any.
pub fn requested_namespace() -> Option<Method> {
    requested_namespace_from(std::env::var(ENV_REQUESTED).ok().as_deref())
}

/// The opt-in's word, read without the environment so the rule can be tested.
fn requested_namespace_from(value: Option<&str>) -> Option<Method> {
    match value {
        Some("bwrap") => Some(Method::Bwrap),
        Some("unshare") => Some(Method::Unshare),
        _ => None,
    }
}

/// Every existing agent workspace under the configuration root's `agents` directory.
///
/// `detect` must see the real workspaces, not only the directory that contains them:
/// an agent's workspace may be a symlink to a parent of the configuration root, and
/// that layout cannot be confined by any mechanism (B9-R-R).
pub fn agent_workspaces(agents_dir: &Path) -> Vec<PathBuf> {
    let mut workspaces = Vec::new();
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return workspaces;
    };
    for entry in entries.flatten() {
        // Follow an agent directory even when it is itself a link: the workspace
        // under it is still a tree the engine will bind or grant.
        let workspace = entry.path().join("workspace");
        if fs::symlink_metadata(&workspace).is_ok() {
            workspaces.push(workspace);
        }
    }
    workspaces.sort();
    workspaces
}

/// Resolve every path a namespace must mount over. A path whose target is missing (a
/// dangling symlink) cannot be placed safely, so the caller must not claim a
/// boundary: `None` (B5-R-R-R).
pub(crate) fn resolved_hide_list(hide: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut resolved = Vec::with_capacity(hide.len());
    for path in hide {
        resolved.push(resolved_layout(path)?);
    }
    resolved.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    resolved.dedup();
    Some(resolved)
}

/// The hide list of one run: the product's roots plus the person's real home.
pub(crate) fn run_hide_roots(plan_hide: &[PathBuf], real_home: Option<&Path>) -> Vec<PathBuf> {
    let mut hide = plan_hide.to_vec();
    if let Some(real_home) = real_home {
        hide.push(real_home.to_path_buf());
    }
    // Keep a dangling symlink: it exists as a name, and where it will point when its
    // target appears is exactly what the canonical overlap check must see (B9-R).
    hide.retain(|path| fs::symlink_metadata(path).is_ok());
    hide.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    hide.dedup();
    hide
}

/// The engine's temporary root, resolved. Every run's private directory is made
/// under it and the namespace mounts are placed through it, so a symlinked `TMPDIR`
/// must not stay lexical (B5-R-R-R).
pub(crate) fn resolved_temp() -> Option<PathBuf> {
    resolved_layout(&std::env::temp_dir())
}

/// The method a run may actually use for this run-time layout.
///
/// `requested` is reduced to [`Method::None`] — never silently raised — when a path
/// to hide cannot be resolved, when a hidden root lies inside the workspace, or when
/// Landlock's allow-list would expose one of them and no opted-in namespace can take
/// over. The second value is the reason when the answer changed, so the refusal can
/// be put on the board and in the run plan instead of being swallowed (B9-R-R).
pub(crate) fn effective_method(
    requested: Method,
    hide: &[PathBuf],
    workspace: &Path,
) -> (Method, Option<&'static str>) {
    if !requested.confined() {
        return (requested, None);
    }
    if resolved_hide_list(hide).is_none() {
        return (Method::None, Some("a path to hide cannot be resolved"));
    }
    let bound = [workspace.to_path_buf()];
    if layout_overlap(hide, &bound).is_some() {
        return (
            Method::None,
            Some("a hidden directory lies inside the run's workspace"),
        );
    }
    if requested == Method::Landlock {
        let mut protected = hide.to_vec();
        let Some(temp) = resolved_temp() else {
            return (Method::None, Some("a path to hide cannot be resolved"));
        };
        protected.push(temp);
        if landlock_overlap(&protected, &bound).is_some() {
            let fallback = choose_namespace(hide);
            if fallback.confined() {
                return (fallback, None);
            }
            return (
                Method::None,
                Some("Landlock's allow-list would expose a hidden directory"),
            );
        }
    }
    (requested, None)
}

/// Choose the strongest confinement this computer can actually provide for the
/// product's roots.
///
/// Landlock is the automatic mechanism. A namespace is tried only when the person
/// opted in with [`ENV_REQUESTED`]; when an opt-in cannot work the answer is
/// [`Method::None`], never a substitute mechanism. The probes run small programs with
/// a short deadline: a namespace that only works on paper is not a confinement, and
/// Landlock is claimed only after a child process proved a fully enforced ruleset
/// that denies a read outside its grant (B10). A namespace needs a staging directory
/// outside every hidden root; without one it is not chosen, because it could not make
/// `/tmp` private.
pub fn detect(
    config_root: &Path,
    state_root: &Path,
    data_root: &Path,
    workspaces: &[PathBuf],
) -> Method {
    let mut hide = vec![
        config_root.to_path_buf(),
        state_root.to_path_buf(),
        data_root.to_path_buf(),
    ];
    if let Some(home) = real_home() {
        hide.push(home);
    }
    // Every run's private root lives under the engine's temporary directory. If that
    // directory is inside a tree Landlock grants (for example `/run/user/<uid>`, a
    // common `TMPDIR`), Landlock would expose a sibling run's askpass file.
    let Some(temp) = resolved_temp() else {
        return Method::None;
    };
    hide.push(temp);
    // The workspaces are the trees a run is actually rooted in; a hidden root inside
    // one of them cannot be hidden by *any* mechanism, because the namespace
    // mechanisms bind the whole workspace back and Landlock grants it. Enumerate the
    // real paths, canonicalize both sides, and never claim a boundary for an overlap
    // (B9-R-R).
    if resolved_hide_list(&hide).is_none() || layout_overlap(&hide, workspaces).is_some() {
        return Method::None;
    }
    if let Some(requested) = requested_namespace() {
        if stage_parent(&hide).is_some() {
            let works = match requested {
                Method::Bwrap => runs_within("bwrap", &["--dev-bind", "/", "/", "/bin/true"]),
                Method::Unshare => unshare_works(),
                _ => false,
            };
            if works {
                return requested;
            }
        }
        // The person asked for this mechanism. Falling back to Landlock would be a
        // different boundary from the one requested, so the honest answer is none
        // (N18); the board then carries the sixth sentence.
        return Method::None;
    }
    // Landlock is claimed only when a child process proved that a ruleset built at
    // `HardRequirement` is fully enforced and denies a read outside the granted tree
    // (B10). An overlap check is cheaper, so it runs first.
    if landlock_overlap(&hide, workspaces).is_none() && landlock_enforced() {
        return Method::Landlock;
    }
    Method::None
}

/// The namespace mechanism the person opted into and that works for these hidden
/// paths, or [`Method::None`]. The run-time guard uses this when a plan names a
/// mechanism for a layout it cannot keep; without an opt-in it never substitutes one.
pub fn choose_namespace(hide: &[PathBuf]) -> Method {
    let Some(requested) = requested_namespace() else {
        return Method::None;
    };
    if stage_parent(hide).is_none() {
        return Method::None;
    }
    let works = match requested {
        Method::Bwrap => runs_within("bwrap", &["--dev-bind", "/", "/", "/bin/true"]),
        Method::Unshare => unshare_works(),
        _ => false,
    };
    if works { requested } else { Method::None }
}

/// The person's real home, which a run must not see.
fn real_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Whether a user and mount namespace can be made **and** its mounts are private.
///
/// A namespace whose mounts still propagate is worse than none: the empty mounts
/// over the product's directories could leak into the host and hide the engine's own
/// files. The probe therefore binds a scratch directory inside the namespace and
/// checks, after the namespace is gone, that the host never saw the bind.
fn unshare_works() -> bool {
    let base = std::env::temp_dir().join(format!(
        "gewu-unshare-probe-{}-{}",
        std::process::id(),
        crate::time::now_unix_ms()
    ));
    let source = base.join("source");
    let target = base.join("target");
    if fs::create_dir_all(&source).is_err() || fs::create_dir_all(&target).is_err() {
        let _ = fs::remove_dir_all(&base);
        return false;
    }
    if fs::write(source.join("marker"), b"inside").is_err() {
        let _ = fs::remove_dir_all(&base);
        return false;
    }
    let script = format!(
        "mount --bind {source} {target} && test -f {target}/marker",
        source = quote(&source.display().to_string()),
        target = quote(&target.display().to_string())
    );
    let ran = runs_within(
        "unshare",
        &[
            "--user",
            "--map-root-user",
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-c",
            &script,
        ],
    );
    // With shared propagation the bind is copied to the host and survives the
    // namespace; the marker would then be visible at the scratch target.
    let leaked = target.join("marker").exists();
    if leaked {
        // Best effort: do not leave a leaked scratch mount behind.
        let _ = Command::new("umount").arg(&target).status();
    }
    let _ = fs::remove_dir_all(&base);
    ran && !leaked
}

/// Whether a program exists and finishes successfully within [`PROBE_TIMEOUT`].
fn runs_within(program: &str, args: &[&str]) -> bool {
    let mut command = Command::new(program);
    command.args(args);
    command_finishes_within(command, PROBE_TIMEOUT)
}

/// Run `command` with null standard streams and a short deadline.
fn command_finishes_within(mut command: Command, timeout: Duration) -> bool {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return false,
        }
    }
}

/// The Landlock access set this kernel can fully enforce, with the ABI it belongs
/// to, or `None` when Landlock is absent or disabled.
///
/// Every candidate ruleset is built with [`CompatLevel::HardRequirement`]. That is
/// the B10 fix: the crate's default `BestEffort` turns an unsupported kernel into a
/// dummy ruleset whose `create()` succeeds, and a rule set that was never enforced
/// would then look available. The highest ABI this kernel actually supports wins; a
/// version that asks for a right the kernel lacks is an error and the next lower one
/// is tried, so an older Landlock kernel gets its own rights rather than a partially
/// enforced set.
fn landlock_accesses() -> Option<(ABI, BitFlags<AccessFs>)> {
    for abi in [
        ABI::V9,
        ABI::V8,
        ABI::V7,
        ABI::V6,
        ABI::V5,
        ABI::V4,
        ABI::V3,
        ABI::V2,
        ABI::V1,
    ] {
        let access = AccessFs::from_all(abi);
        let built = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(access);
        if built.is_ok_and(|ruleset| ruleset.create().is_ok()) {
            return Some((abi, access));
        }
    }
    None
}

/// Whether Landlock really confines a process on this computer.
///
/// The ruleset is created and applied in a **child process**, because `restrict_self`
/// cannot be undone and the board itself must stay free. The child must report
/// [`RulesetStatus::FullyEnforced`], read an allowed control file, and be denied a
/// file outside the grant; anything else — a missing kernel feature, a disabled
/// kernel, a partially enforced set — means `false`, and the board says `runs are not
/// confined on this computer` (B10).
pub(crate) fn landlock_enforced() -> bool {
    #[cfg(test)]
    if landlock_probe_binary().is_none() {
        // The library's unit tests run under libtest, which knows no
        // `--landlock-probe`. With no product binary built beside it, the
        // compatibility check is still strict (HardRequirement), and the real
        // child-process proof is run by tests/confinement.rs against the product.
        return landlock_accesses().is_some();
    }
    let Some(binary) = landlock_probe_binary() else {
        return false;
    };
    let root = std::env::temp_dir().join(format!(
        "gewu-landlock-probe-{}-{}",
        std::process::id(),
        crate::time::now_unix_ms()
    ));
    let allowed = root.join("allowed");
    let wrote = fs::create_dir_all(&allowed).is_ok()
        && fs::write(allowed.join("ok"), b"ok").is_ok()
        && fs::write(root.join("secret"), PROBE_SECRET).is_ok();
    if !wrote {
        let _ = fs::remove_dir_all(&root);
        return false;
    }
    let mut command = Command::new(binary);
    command.arg("--landlock-probe").arg(&root);
    let enforced = command_finishes_within(command, PROBE_TIMEOUT);
    let _ = fs::remove_dir_all(&root);
    enforced
}

/// The product binary that understands `--landlock-probe`.
fn landlock_probe_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Cargo's test binaries live in `target/<profile>/deps/`; the product binary is
    // one directory above. A product binary is used under its own name, whatever it
    // was renamed to, because it is the program the person is running.
    let in_deps = exe
        .parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == "deps");
    if !in_deps {
        return Some(exe);
    }
    let sibling = exe.parent()?.parent()?.join("gewu-multiverse");
    sibling.is_file().then_some(sibling)
}

/// The `--landlock-probe` entry: prove in this child that a ruleset is fully
/// enforced before the board or a run claims Landlock (B10).
///
/// The parent writes `allowed/ok` and a `secret` beside it. The child applies a
/// ruleset that grants only `allowed`, requires [`RulesetStatus::FullyEnforced`],
/// reads the allowed file (the positive control) and must then be denied the secret.
/// It never prints the secret.
pub fn run_landlock_probe(root: &Path) -> i32 {
    match probe_landlock(root) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("gewu-multiverse: {reason}");
            126
        }
    }
}

fn probe_landlock(root: &Path) -> Result<(), String> {
    let Some((abi, access)) = landlock_accesses() else {
        return Err(
            "runs are not confined on this computer — Landlock is not available".to_owned(),
        );
    };
    let read = AccessFs::from_read(abi);
    let allowed = root.join("allowed");
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(access)
        .and_then(|ruleset| ruleset.create())
        .map_err(|error| {
            format!(
                "runs are not confined on this computer — the Landlock ruleset cannot be made: {error}"
            )
        })?;
    let status = ruleset
        .add_rules(landlock::path_beneath_rules([&allowed], read))
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|error| {
            format!(
                "runs are not confined on this computer — the Landlock rules cannot be applied: {error}"
            )
        })?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "runs are not confined on this computer — the Landlock ruleset was not fully enforced ({:?})",
            status.ruleset
        ));
    }
    // The positive control proves this is a working ruleset, not a blanket refusal
    // that would make an unrelated error look like confinement.
    fs::read_to_string(allowed.join("ok")).map_err(|error| {
        format!(
            "runs are not confined on this computer — the allowed probe file is not readable: {error}"
        )
    })?;
    match fs::read_to_string(root.join("secret")) {
        Ok(_) => Err(
            "runs are not confined on this computer — the ruleset did not deny the probe read"
                .to_owned(),
        ),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(format!(
            "runs are not confined on this computer — the ruleset did not deny the probe read ({error})"
        )),
    }
}

/// The trees [`exec_landlock`] allows a run to read, on top of the run's own
/// workspace, `TMPDIR` and `HOME`.
fn landlock_read_trees() -> Vec<PathBuf> {
    let mut trees: Vec<PathBuf> = LANDLOCK_READ_TREES.iter().map(PathBuf::from).collect();
    // The product binary is `GIT_ASKPASS`; its directory must be executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            trees.push(parent.to_path_buf());
        }
    }
    trees
}

/// A hidden root that Landlock's allow-list would nevertheless expose, if any.
///
/// Landlock grants a whole tree; it cannot subtract the configuration root from
/// `/run` or from a workspace. A hidden path that lies **inside** an allowed tree is
/// therefore an overlap. The reverse (the workspace inside the hidden configuration
/// root) is the intended carve-out and is not a conflict.
///
/// Both sides are canonicalized before the comparison (B9-R): a hidden state root
/// that is a symlink into the workspace is inside it even when the lexical path is
/// not. A hidden path that cannot be resolved — a dangling symlink — is somewhere
/// unknown, so it is reported as an overlap and never called confined.
pub fn landlock_overlap(hide: &[PathBuf], extra_allow: &[PathBuf]) -> Option<PathBuf> {
    let mut allowed = landlock_read_trees();
    allowed.extend(LANDLOCK_WRITE_TREES.iter().map(PathBuf::from));
    // A path may reach an allowed tree through a symlink. Resolve what exists so a
    // `--config-dir` behind a link into `/run` is still seen as an overlap.
    let mut trees: Vec<PathBuf> = allowed
        .iter()
        .filter_map(|path| resolved_layout(path))
        .collect();
    for path in extra_allow {
        // The caller's own read trees (the workspace, the product binary's
        // directory) are part of the grant. One that cannot be resolved is an
        // unknown boundary, never a free pass.
        match resolved_layout(path) {
            Some(path) => trees.push(path),
            None => return Some(path.clone()),
        }
    }
    for hidden in hide {
        let Some(hidden) = resolved_layout(hidden) else {
            return Some(hidden.clone());
        };
        for tree in &trees {
            if hidden == *tree || hidden.starts_with(tree) {
                return Some(hidden);
            }
        }
    }
    None
}

/// A hidden path that one of `bound`'s paths would hand back, if any.
///
/// This is the check the namespace mechanisms need: they bind the workspace (and
/// this run's own root) back *after* the hides, so a hidden root that resolves
/// inside one of those trees would reappear with it (B9-R). Both sides are
/// canonicalized first; an unresolvable path is a conflict, because nothing proves
/// it is outside.
pub fn layout_overlap(hide: &[PathBuf], bound: &[PathBuf]) -> Option<PathBuf> {
    let mut trees = Vec::with_capacity(bound.len());
    for path in bound {
        match resolved_layout(path) {
            Some(path) => trees.push(path),
            None => return Some(path.clone()),
        }
    }
    for hidden in hide {
        let Some(hidden) = resolved_layout(hidden) else {
            return Some(hidden.clone());
        };
        if trees
            .iter()
            .any(|tree| hidden == *tree || hidden.starts_with(tree))
        {
            return Some(hidden);
        }
    }
    None
}

/// Resolve a path the way `realpath` does, without requiring the leaf to exist.
///
/// `fs::canonicalize` follows every symlink but fails when the leaf is missing,
/// which is normal for a configuration root before the first open. Resolve the
/// longest existing prefix and append the missing components. A symlink whose
/// target does not exist is deliberately *not* resolved lexically: what it will
/// point to when the target appears is unknown, so [`layout_overlap`] and
/// [`landlock_overlap`] treat it as a conflict (B9-R).
pub(crate) fn resolved_layout(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Some(canonical);
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return None;
    }
    let name = path.file_name()?;
    let parent = path.parent()?;
    if parent == path {
        return None;
    }
    if parent.as_os_str().is_empty() {
        // A relative one-component path: resolve it against the current directory.
        return fs::canonicalize(".").ok().map(|cwd| cwd.join(name));
    }
    Some(resolved_layout(parent)?.join(name))
}

/// A directory outside every hidden tree in which the namespace mounts can stage
/// this run's own files across the `tmpfs` hides.
fn stage_parent(hide: &[PathBuf]) -> Option<PathBuf> {
    // The staging directory must survive the private tmpfs over the engine's
    // temporary root; a temporary root of `/` cannot be replaced safely at all.
    let temp = resolved_temp()?;
    if temp == Path::new("/") {
        return None;
    }
    // Every hidden root is resolved before the containment test: a staging candidate
    // under the *target* of a symlinked hide is not safe either (B5-R-R-R).
    let hidden: Vec<PathBuf> = hide
        .iter()
        .map(|path| resolved_layout(path))
        .collect::<Option<_>>()?;
    let mut candidates = vec![PathBuf::from("/dev/shm")];
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime));
    }
    candidates.push(PathBuf::from("/var/tmp"));
    for candidate in candidates {
        if !candidate.is_dir() || candidate.starts_with("/tmp") || candidate.starts_with(&temp) {
            continue;
        }
        // The stage itself must survive every hide. A candidate under a hidden root
        // would disappear; a hidden root under the candidate is a sibling and is fine.
        if hidden.iter().any(|root| candidate.starts_with(root)) {
            continue;
        }
        let probe = candidate.join(format!(
            ".gewu-stage-probe-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        if fs::create_dir(&probe).is_err() {
            continue;
        }
        // The staging directory must also allow execution: the askpass binary is
        // bound through it, and a `noexec` tmpfs (some `/dev/shm` mounts) would
        // inherit that flag onto the restored binary and break git authentication.
        let executable = probe.join("probe");
        let executable_ok = fs::write(&executable, b"#!/bin/sh\nexit 0\n").is_ok()
            && crate::mode::set_mode(&executable, 0o700).is_ok()
            && Command::new(&executable)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
        let _ = fs::remove_dir_all(&probe);
        if executable_ok {
            return Some(candidate);
        }
    }
    None
}

/// Create this run's staging directory: a private place outside every hidden root
/// holding the mount targets the namespace mechanisms bind from after the hides.
pub fn create_stage(hide: &[PathBuf], run_id: &str) -> Result<PathBuf, String> {
    let parent = stage_parent(hide).ok_or_else(|| {
        "no private staging directory is available outside the product's files".to_owned()
    })?;
    let safe_run: String = run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let stage = parent.join(format!(
        "gewu-stage-{}-{safe_run}-{}",
        std::process::id(),
        crate::time::now_unix_ms()
    ));
    fs::create_dir(&stage).map_err(|error| {
        format!(
            "the confined shell's staging directory {} cannot be made: {error}",
            stage.display()
        )
    })?;
    let _ = crate::mode::set_mode(&stage, crate::mode::PRIVATE_DIR_MODE);
    for name in ["workspace", "root"] {
        fs::create_dir(stage.join(name)).map_err(|error| {
            format!(
                "the confined shell's staging directory {} cannot be made: {error}",
                stage.display()
            )
        })?;
    }
    crate::mode::write_private(stage.join("askpass-exe"), b"").map_err(|error| {
        format!(
            "the confined shell's staging file in {} cannot be made: {error}",
            stage.display()
        )
    })?;
    Ok(stage)
}

/// The paths one namespace invocation needs. Kept as one value so the `bwrap`
/// argument list and the `unshare` script can be built and tested without the
/// environment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NamespacePlan<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) hide: &'a [PathBuf],
    pub(crate) root: &'a Path,
    pub(crate) stage: &'a Path,
    pub(crate) temp: &'a Path,
    pub(crate) exe: Option<&'a Path>,
}

/// The `--confined-shell` entry: apply the method named in the environment, then
/// become `/bin/sh -c <command>`. Returns only on failure, as a process exit code.
pub fn run_confined_shell() -> i32 {
    let method = Method::parse(&std::env::var(ENV_METHOD).unwrap_or_default());
    let command = std::env::var(ENV_COMMAND).unwrap_or_default();
    let workspace = std::env::var_os(ENV_WORKSPACE).map(PathBuf::from);
    let hide: Vec<PathBuf> = std::env::var(ENV_HIDE)
        .unwrap_or_default()
        .split(':')
        .filter(|part| !part.is_empty())
        .map(PathBuf::from)
        .collect();
    let workspace = match workspace {
        Some(workspace) => workspace,
        None => {
            eprintln!("gewu-multiverse: the confined shell has no workspace");
            return 126;
        }
    };
    let exe = std::env::current_exe().ok();
    let root = std::env::var_os(ENV_ROOT).map(PathBuf::from);
    let stage = std::env::var_os(ENV_STAGE).map(PathBuf::from);
    // A namespace may only place mounts on resolved paths: bubblewrap refuses a
    // symlink destination, and a hide under `/tmp` whose target is elsewhere would
    // otherwise stay visible behind the private `/tmp` (B5-R-R-R). The engine has
    // already resolved these; the entry repeats it so a direct invocation cannot
    // build an unsafe list.
    let namespace_hide = if method.namespace() {
        match resolved_hide_list(&hide) {
            Some(hide) => hide,
            None => {
                eprintln!("gewu-multiverse: a path to hide cannot be resolved");
                return 126;
            }
        }
    } else {
        hide
    };
    // Resolve the run's own temporary root as well: bwrap would refuse a tmpfs over
    // a symlinked destination.
    let namespace_temp = if method.namespace() {
        let Some(root) = root.as_deref() else {
            eprintln!("gewu-multiverse: the confined shell has no private root");
            return 126;
        };
        match root.parent().and_then(resolved_layout) {
            Some(temp) => Some(temp),
            None => {
                eprintln!(
                    "gewu-multiverse: the confined shell's temporary root cannot be resolved"
                );
                return 126;
            }
        }
    } else {
        None
    };
    let workspace = if method.namespace() {
        // The workspace is a mount destination too; resolve it once, so neither
        // bubblewrap nor the `cd` sees a symlink.
        match resolved_layout(&workspace) {
            Some(workspace) => workspace,
            None => {
                eprintln!("gewu-multiverse: the confined shell's workspace cannot be resolved");
                return 126;
            }
        }
    } else {
        workspace
    };
    let namespace_plan = if let Some(temp) = namespace_temp.as_deref() {
        let (Some(root), Some(stage)) = (root.as_deref(), stage.as_deref()) else {
            eprintln!("gewu-multiverse: the confined shell has no private root");
            return 126;
        };
        Some(NamespacePlan {
            workspace: &workspace,
            hide: &namespace_hide,
            root,
            stage,
            temp,
            exe: exe.as_deref(),
        })
    } else {
        None
    };
    if !method.namespace() {
        if let Err(error) = std::env::set_current_dir(&workspace) {
            eprintln!(
                "gewu-multiverse: the confined shell cannot enter {}: {error}",
                workspace.display()
            );
            return 126;
        }
    }
    let result = match method {
        Method::Bwrap => exec_bwrap(namespace_plan.expect("a namespace plan"), &command),
        Method::Unshare => exec_unshare(namespace_plan.expect("a namespace plan"), &command),
        Method::Landlock => exec_landlock(&workspace, &command),
        Method::None => exec_plain(&command),
    };
    match result {
        Ok(never) => match never {},
        Err(error) => {
            eprintln!("gewu-multiverse: the confined shell could not start: {error}");
            126
        }
    }
}

/// A never-returning successful exec, made a value so every branch has one type.
fn exec_plain(command: &str) -> io::Result<std::convert::Infallible> {
    Err(exec_sh(command))
}

/// Replace this process with `/bin/sh -c command`.
fn exec_sh(command: &str) -> io::Error {
    Command::new("/bin/sh").arg("-c").arg(command).exec()
}

/// Landlock: allow the workspace, the system tools and `/dev`, deny everything else.
///
/// The caller only chooses this method when [`landlock_overlap`] found no hidden
/// root inside one of these trees; otherwise a rule for `/run` or the workspace
/// would hand back the configuration root or the Forgejo data root.
fn exec_landlock(workspace: &Path, command: &str) -> io::Result<std::convert::Infallible> {
    // The same HardRequirement build as the probe: a kernel without Landlock must
    // never hand back a dummy ruleset, and only a fully enforced set may execute the
    // command (B10). Otherwise the command is refused; `run_confined_shell` prints
    // the sentence and exits 126, and the run never executes it unconfined.
    let (abi, all) = landlock_accesses().ok_or_else(|| {
        io::Error::other("runs are not confined on this computer — Landlock is not available")
    })?;
    let read = AccessFs::from_read(abi);
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(all)
        .and_then(|ruleset| ruleset.create())
        .map_err(|error| {
            io::Error::other(format!("the Landlock ruleset cannot be made: {error}"))
        })?;
    let mut rules = Vec::new();
    for path in landlock_read_trees() {
        if path.exists() {
            rules.extend(landlock::path_beneath_rules([path], read));
        }
    }
    let mut writable = vec![workspace.to_path_buf(), PathBuf::from("/dev")];
    writable.extend(LANDLOCK_WRITE_TREES.iter().skip(1).map(PathBuf::from));
    if let Some(temp) = std::env::var_os("TMPDIR") {
        writable.push(PathBuf::from(temp));
    }
    if let Some(home) = std::env::var_os("HOME") {
        writable.push(PathBuf::from(home));
    }
    for path in writable {
        if path.exists() {
            rules.extend(landlock::path_beneath_rules([path], all));
        }
    }
    let status = ruleset
        .add_rules(rules)
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|error| {
            io::Error::other(format!("the Landlock rules cannot be applied: {error}"))
        })?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(io::Error::other(format!(
            "runs are not confined on this computer — the Landlock ruleset was not fully enforced ({:?})",
            status.ruleset
        )));
    }
    Err(exec_sh(command))
}

/// bubblewrap: bind the whole system, hide the protected directories with empty
/// mounts, and put this run's own directories back at their own paths.
fn exec_bwrap(plan: NamespacePlan<'_>, command: &str) -> io::Result<std::convert::Infallible> {
    let args = bwrap_args(&plan, command);
    Err(Command::new("bwrap").args(args).exec())
}

/// Whether one of the path-replacing mounts removes a path from the namespace.
fn hidden_by_namespace(path: &Path, hide: &[PathBuf], temp: &Path) -> bool {
    path.starts_with("/tmp")
        || path.starts_with(temp)
        || hide.iter().any(|root| path.starts_with(root))
}

/// The `bwrap` argument list, kept pure so a test can read it.
///
/// The order matters: the workspace, this run's root and the askpass binary are
/// staged in a directory outside the hides first; then `/tmp` and the engine's
/// `TMPDIR` become private `tmpfs` mounts; then the product's roots are replaced;
/// then exactly this run's root, its workspace and (only when it was hidden) the
/// askpass binary are bound back. Nothing of a sibling run is bound back.
fn bwrap_args(plan: &NamespacePlan<'_>, command: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["--dev-bind".to_owned(), "/".to_owned(), "/".to_owned()];
    args.extend([
        "--bind".to_owned(),
        plan.workspace.display().to_string(),
        plan.stage.join("workspace").display().to_string(),
    ]);
    args.extend([
        "--bind".to_owned(),
        plan.root.display().to_string(),
        plan.stage.join("root").display().to_string(),
    ]);
    if let Some(exe) = plan.exe {
        args.extend([
            "--ro-bind".to_owned(),
            exe.display().to_string(),
            plan.stage.join("askpass-exe").display().to_string(),
        ]);
    }
    // A private `/tmp` is the point of B8: the engine's per-run roots live under it,
    // and a sibling's askpass file must not be visible.
    args.extend(["--tmpfs".to_owned(), "/tmp".to_owned()]);
    if plan.temp != Path::new("/tmp") && !plan.temp.starts_with("/tmp") {
        args.extend(["--tmpfs".to_owned(), plan.temp.display().to_string()]);
    }
    for path in plan.hide {
        if path.exists() && !path.starts_with("/tmp") && !path.starts_with(plan.temp) {
            args.extend(["--tmpfs".to_owned(), path.display().to_string()]);
        }
    }
    // Exactly this run's root comes back, not the shared parent.
    args.extend([
        "--dir".to_owned(),
        plan.root.display().to_string(),
        "--bind".to_owned(),
        plan.stage.join("root").display().to_string(),
        plan.root.display().to_string(),
    ]);
    args.extend([
        "--dir".to_owned(),
        plan.workspace.display().to_string(),
        "--bind".to_owned(),
        plan.stage.join("workspace").display().to_string(),
        plan.workspace.display().to_string(),
    ]);
    if let Some(exe) = plan.exe {
        if hidden_by_namespace(exe, plan.hide, plan.temp) {
            if let Some(parent) = exe.parent() {
                args.extend(["--dir".to_owned(), parent.display().to_string()]);
            }
            // `--ro-bind` creates the destination file node when it is missing
            // (`ensure_node_at` in bubblewrap). `--file` is `--file FD DEST`, not a
            // path, and passing one makes bwrap reject the whole invocation
            // (B5-R-R). The parent was just created by `--dir`.
            args.extend([
                "--ro-bind".to_owned(),
                plan.stage.join("askpass-exe").display().to_string(),
                exe.display().to_string(),
            ]);
        }
    }
    args.extend([
        "--chdir".to_owned(),
        plan.workspace.display().to_string(),
        "--".to_owned(),
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        command.to_owned(),
    ]);
    args
}

/// util-linux `unshare`: a user and mount namespace whose mounts are private, with a
/// private `/tmp` and engine `TMPDIR`, then empty tmpfs mounts over every protected
/// directory (deepest first), then exactly this run's own paths bound back.
///
/// The propagation is made private twice: by `--propagation private`, which applies
/// before the first mount, and by `mount --make-rprivate /` as the script's first
/// command. Either alone would do; a host where one spelling is unavailable still
/// gets the other, and neither can let an empty mount reach the host.
fn exec_unshare(plan: NamespacePlan<'_>, _command: &str) -> io::Result<std::convert::Infallible> {
    let script = unshare_script(&plan);
    Err(Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--propagation",
            "private",
            "/bin/sh",
            "-c",
        ])
        .arg(script)
        .exec())
}

/// The mount script `unshare` runs, kept pure so a test can read it.
fn unshare_script(plan: &NamespacePlan<'_>) -> String {
    let stage_workspace = plan.stage.join("workspace");
    let stage_root = plan.stage.join("root");
    let stage_exe = plan.stage.join("askpass-exe");
    let mut script = String::from("set -eu\nmount --make-rprivate /\n");
    // The staging targets must exist before the first bind: mount(8) refuses a
    // missing mount point (B5-R). They live outside every hide, so they survive.
    script.push_str(&format!(
        "mkdir -p {} {} {}\n",
        quote(&plan.stage.display().to_string()),
        quote(&stage_workspace.display().to_string()),
        quote(&stage_root.display().to_string())
    ));
    script.push_str(&format!(
        "mount --bind {} {}\n",
        quote(&plan.workspace.display().to_string()),
        quote(&stage_workspace.display().to_string())
    ));
    script.push_str(&format!(
        "mount --bind {} {}\n",
        quote(&plan.root.display().to_string()),
        quote(&stage_root.display().to_string())
    ));
    if let Some(exe) = plan.exe {
        script.push_str(&format!(
            ": > {}\nmount --bind {} {}\n",
            quote(&stage_exe.display().to_string()),
            quote(&exe.display().to_string()),
            quote(&stage_exe.display().to_string())
        ));
    }
    // A fresh private `/tmp`, then the engine's own temporary root when it is
    // somewhere else. The engine keeps every run under that root, so replacing it is
    // what hides the sibling `gewu-run-*` directories.
    script.push_str("mkdir -p /tmp\nmount -t tmpfs tmpfs /tmp\n");
    if plan.temp != Path::new("/tmp") && !plan.temp.starts_with("/tmp") {
        script.push_str(&format!(
            "mkdir -p {}\nmount -t tmpfs tmpfs {}\n",
            quote(&plan.temp.display().to_string()),
            quote(&plan.temp.display().to_string())
        ));
    }
    // Deepest first: a protected directory may sit inside the person's home, which
    // is itself replaced by an empty mount; the inner mount must be made while its
    // name still exists.
    let mut ordered: Vec<&PathBuf> = plan.hide.iter().collect();
    ordered.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    ordered.dedup();
    for path in ordered {
        // A path under the fresh `/tmp` no longer has a name; the filesystem it was
        // on is already gone, so a marker directory is enough to hide it again.
        script.push_str(&format!(
            "mkdir -p {}\nmount -t tmpfs tmpfs {}\n",
            quote(&path.display().to_string()),
            quote(&path.display().to_string())
        ));
    }
    // Put this run's own root back, then its workspace; a sibling root is never
    // mentioned, so it stays behind the private tmpfs.
    script.push_str(&format!(
        "mkdir -p {}\nmount --bind {} {}\n",
        quote(&plan.root.display().to_string()),
        quote(&stage_root.display().to_string()),
        quote(&plan.root.display().to_string())
    ));
    script.push_str(&format!(
        "mkdir -p {}\nmount --bind {} {}\n",
        quote(&plan.workspace.display().to_string()),
        quote(&stage_workspace.display().to_string()),
        quote(&plan.workspace.display().to_string())
    ));
    if let Some(exe) = plan.exe {
        if hidden_by_namespace(exe, plan.hide, plan.temp) {
            if let Some(parent) = exe.parent() {
                script.push_str(&format!(
                    "mkdir -p {}\n",
                    quote(&parent.display().to_string())
                ));
            }
            // Create the mount point only because the hides removed the binary;
            // a binary that is still visible is never truncated (B5-R).
            script.push_str(&format!(
                "[ -e {} ] || : > {}\nmount --bind {} {}\n",
                quote(&exe.display().to_string()),
                quote(&exe.display().to_string()),
                quote(&stage_exe.display().to_string()),
                quote(&exe.display().to_string())
            ));
        }
    }
    script.push_str(&format!(
        "cd {}\nexec /bin/sh -c \"$GEWU_CONFINE_COMMAND\"\n",
        quote(&plan.workspace.display().to_string())
    ));
    script
}

/// Quote one path for a POSIX shell single-quoted word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The quote helper, for the tests.
#[cfg(test)]
pub(crate) fn quoted(text: &str) -> String {
    quote(text)
}

/// The parent environment a run may inherit: a short allow-list, not the person's
/// whole environment (N13). `HOME` and `TMPDIR` are added by the run with the run's
/// own directories; the `GEWU_*` and git variables are added explicitly.
pub(crate) fn safe_environment<I>(vars: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut kept = Vec::new();
    for (key, value) in vars {
        let name = key.to_string_lossy();
        let allowed =
            matches!(name.as_ref(), "PATH" | "TERM" | "LANG" | "TZ") || name.starts_with("LC_");
        if allowed {
            kept.push((key, value));
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        crate::test_support::scratch_root(tag)
    }

    #[test]
    fn every_method_round_trips_and_none_is_not_confined() {
        for method in [
            Method::Bwrap,
            Method::Unshare,
            Method::Landlock,
            Method::None,
        ] {
            assert_eq!(Method::parse(method.as_str()), method);
        }
        assert!(Method::Bwrap.confined());
        assert!(Method::Landlock.confined());
        assert!(!Method::None.confined());
        assert_eq!(Method::parse("something else"), Method::None);
    }

    #[test]
    fn a_path_with_a_space_or_a_quote_survives_the_shell_word() {
        assert_eq!(quoted("plain"), "'plain'");
        assert_eq!(quoted("a b"), "'a b'");
        assert_eq!(quoted("a'b"), "'a'\\''b'");
    }

    #[test]
    fn a_landlock_hidden_root_is_detected_under_each_allowed_tree() {
        for root in [
            "/run/user/1000/gewu",
            "/etc/gewu",
            "/opt/gewu",
            "/usr/local/gewu",
            "/dev/shm/gewu",
            "/proc/self/gewu",
            "/sys/gewu",
        ] {
            let hidden = PathBuf::from(root);
            assert!(
                landlock_overlap(std::slice::from_ref(&hidden), &[]).is_some(),
                "Landlock would expose {root}"
            );
        }
        assert!(
            landlock_overlap(&[PathBuf::from("/var/lib/gewu")], &[]).is_none(),
            "an ordinary path outside the allow-list is not an overlap"
        );
        // A hidden root inside a workspace is the B9 case the reviewer names.
        let workspace = PathBuf::from("/tmp/me/agents/agent-1/workspace");
        let hidden = workspace.join("state");
        assert!(
            landlock_overlap(
                std::slice::from_ref(&hidden),
                std::slice::from_ref(&workspace)
            )
            .is_some(),
            "Landlock would expose a root inside the workspace"
        );
        // The ordinary layout is not an overlap: the workspace is inside the hidden
        // configuration root, which is the intended carve-out.
        let config = PathBuf::from("/home/me/.config/gewu-multiverse");
        let ordinary_workspace = config.join("agents/agent-1/workspace");
        assert!(landlock_overlap(&[config], &[ordinary_workspace]).is_none());
    }

    #[test]
    fn a_hidden_root_inside_the_binary_directory_overlaps_landlock() {
        // Landlock grants the product binary's own directory so `GIT_ASKPASS` can
        // run; a hidden root there would come back with that grant. The directory
        // is resolved before the comparison like every other allow-listed tree.
        let exe = std::env::current_exe().expect("the test binary");
        let hidden = exe
            .parent()
            .expect("the test binary's directory")
            .join("gewu-hidden-root");
        assert!(
            landlock_overlap(&[hidden], &[]).is_some(),
            "Landlock would grant the product binary's directory and the hidden root"
        );
    }

    #[test]
    fn a_symlinked_hidden_root_inside_the_agents_directory_is_unconfined() {
        // B9-R: the lexical state root is beside the configuration root, but the
        // symlink points into an agent's workspace, which the namespaces bind back.
        let root = scratch("confinement-symlink-state");
        let config = root.join("config");
        let data = root.join("data");
        let workspace = config.join("agents/agent-1/workspace");
        let target = workspace.join("state");
        fs::create_dir_all(&target).expect("the workspace target");
        fs::create_dir_all(&data).expect("the data root");
        let state = root.join("state-link");
        std::os::unix::fs::symlink(&target, &state).expect("the state symlink");
        assert_eq!(
            detect(&config, &state, &data, std::slice::from_ref(&workspace)),
            Method::None,
            "a symlinked state root inside the workspace was called confined"
        );
        assert!(
            layout_overlap(
                std::slice::from_ref(&state),
                std::slice::from_ref(&workspace)
            )
            .is_some(),
            "the canonical check missed the symlink"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_workspace_over_the_config_root_is_unconfined() {
        // B9-R: the workspace path is a symlink to a parent of the configuration
        // root, so binding the workspace back would hand the records to the run.
        let root = scratch("confinement-symlink-workspace");
        let config = root.join("config");
        let state = root.join("state");
        let data = root.join("data");
        for path in [&config, &state, &data] {
            fs::create_dir_all(path).expect("a root");
        }
        let workspace = root.join("work-link");
        std::os::unix::fs::symlink(&root, &workspace).expect("the workspace symlink");
        assert_eq!(
            detect(&config, &state, &data, std::slice::from_ref(&workspace)),
            Method::None,
            "a workspace symlinked over the config root was called confined"
        );
        assert!(
            layout_overlap(
                std::slice::from_ref(&config),
                std::slice::from_ref(&workspace)
            )
            .is_some(),
            "the canonical check missed the symlinked workspace"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dangling_symlink_is_never_called_confined() {
        // B9-R: a dangling link has no target to resolve. It could point inside the
        // workspace as soon as the target is created, so it must not be called
        // confined; the reverse (a link to a disjoint existing target) is fine.
        let root = scratch("confinement-dangling");
        let config = root.join("config");
        let state = root.join("state");
        let data = root.join("data");
        for path in [&config, &state, &data] {
            fs::create_dir_all(path).expect("a root");
        }
        let dangling = root.join("dangling-link");
        std::os::unix::fs::symlink(root.join("not-created-yet"), &dangling).expect("the link");
        assert_eq!(
            detect(&config, &dangling, &data, &[]),
            Method::None,
            "a dangling state symlink was called confined"
        );
        assert!(
            layout_overlap(std::slice::from_ref(&dangling), &[]).is_some(),
            "an unresolved hidden path must be a conflict"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_run_environment_is_a_safe_allow_list() {
        let parent: Vec<(OsString, OsString)> = [
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("TERM", "xterm-256color"),
            ("TZ", "UTC"),
            ("OPENAI_API_KEY", "sk-should-not-be-inherited"),
            ("AWS_SECRET_ACCESS_KEY", "aws-should-not-be-inherited"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("HARNESS_SESSION_ID", "session-should-not-be-inherited"),
            ("HOME", "/home/me"),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect();
        let kept = safe_environment(parent);
        let names: Vec<String> = kept
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for wanted in ["PATH", "LANG", "LC_ALL", "TERM", "TZ"] {
            assert!(names.iter().any(|name| name == wanted), "{names:?}");
        }
        for secret in [
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "HARNESS_SESSION_ID",
            "HOME",
        ] {
            assert!(!names.iter().any(|name| name == secret), "{names:?}");
        }
    }

    /// The plan a test hands to the script builder.
    struct Fixture {
        root: PathBuf,
        workspace: PathBuf,
        hide: Vec<PathBuf>,
        temp: PathBuf,
        stage: PathBuf,
        run_root: PathBuf,
        sibling: PathBuf,
        exe: PathBuf,
        fake_bin: PathBuf,
        log: PathBuf,
    }

    fn fixture(tag: &str) -> Fixture {
        let root = scratch(tag);
        let workspace = root.join("config/agents/agent-1/workspace");
        let hide = vec![root.join("config"), root.join("state"), root.join("data")];
        let temp = root.join("engine-tmp");
        let stage = root.join("stage");
        let run_root = temp.join("gewu-run-1");
        // A sibling run, with the file B8 is about.
        let sibling = temp.join("gewu-run-2");
        let exe = root.join("bin/gewu-multiverse");
        let fake_bin = root.join("fake-bin");
        let log = root.join("mounts.log");
        for path in [&workspace, &temp, &stage, &run_root, &sibling] {
            fs::create_dir_all(path).expect("the fixture directory");
        }
        fs::create_dir_all(run_root.join("tmp")).expect("the run tmp");
        fs::create_dir_all(sibling.join("tmp")).expect("the sibling tmp");
        fs::write(run_root.join("tmp/askpass.toml"), "mine").expect("my askpass");
        fs::write(sibling.join("tmp/askpass.toml"), "theirs").expect("the sibling askpass");
        fs::create_dir_all(exe.parent().expect("a parent")).expect("the bin directory");
        fs::write(&exe, b"#!/bin/sh\nexit 0\n").expect("the binary");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).expect("the binary mode");
        fs::create_dir_all(&fake_bin).expect("the fake bin");
        write_fake(&fake_bin, "mount", FAKE_MOUNT);
        write_fake(&fake_bin, "umount", "#!/bin/sh\nexit 0\n");
        Fixture {
            root,
            workspace,
            hide,
            temp,
            stage,
            run_root,
            sibling,
            exe,
            fake_bin,
            log,
        }
    }

    fn write_fake(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).expect("the fake program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("the fake mode");
    }

    /// A `mount` that records its arguments and refuses a missing mount point, so a
    /// script that binds before creating its targets fails the way mount(8) does.
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

    fn plan<'a>(fixture: &'a Fixture, temp: &'a Path, exe: Option<&'a Path>) -> NamespacePlan<'a> {
        NamespacePlan {
            workspace: &fixture.workspace,
            hide: &fixture.hide,
            root: &fixture.run_root,
            stage: &fixture.stage,
            temp,
            exe,
        }
    }

    fn run_script(fixture: &Fixture, script: &str) -> std::process::Output {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(&fixture.root)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fixture.fake_bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("GEWU_FAKE_MOUNT_LOG", &fixture.log)
            .env("GEWU_CONFINE_COMMAND", "true")
            .output()
            .expect("the script runs")
    }

    #[test]
    fn the_complete_unshare_script_runs_under_a_fake_mount() {
        let fixture = fixture("confinement-complete-script");
        let script = unshare_script(&plan(&fixture, &fixture.temp, Some(&fixture.exe)));
        let output = run_script(&fixture, &script);
        assert!(
            output.status.success(),
            "the complete script failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&fixture.log).expect("the mount log");
        // The private tmpfs over `/tmp` and the engine's TMPDIR, and this run's own
        // root bound back after both.
        assert!(log.contains("-t tmpfs tmpfs /tmp"), "{log}");
        if !fixture.temp.starts_with("/tmp") {
            assert!(
                log.contains(&format!("-t tmpfs tmpfs {}", fixture.temp.display())),
                "{log}"
            );
        }
        assert!(
            log.contains(&format!(
                "--bind {} {}",
                fixture.stage.join("root").display(),
                fixture.run_root.display()
            )),
            "{log}"
        );
        // A sibling run is never bound and never named.
        assert!(
            !log.contains(&fixture.sibling.display().to_string()),
            "{log}"
        );
        // The askpass binary is restored only because it sits under a hidden root
        // here (the fixture's `bin` is under `root`, not under a hide), so it is
        // still visible and must not be truncated. This is the B5-R second defect.
        assert!(
            !fs::read(&fixture.exe)
                .expect("the binary is still there")
                .is_empty(),
            "the script truncated a binary that was not hidden"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_complete_unshare_script_restores_a_hidden_askpass_binary() {
        let fixture = fixture("confinement-hidden-binary");
        // Put the binary inside the configuration root, which is hidden.
        let exe = fixture.hide[0].join("bin/gewu-multiverse");
        fs::create_dir_all(exe.parent().expect("a parent")).expect("the bin directory");
        fs::write(&exe, b"#!/bin/sh\nexit 0\n").expect("the binary");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).expect("the mode");
        let script = unshare_script(&plan(&fixture, &fixture.temp, Some(&exe)));
        let output = run_script(&fixture, &script);
        assert!(
            output.status.success(),
            "the complete script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&fixture.log).expect("the mount log");
        assert!(
            log.contains(&format!(
                "--bind {} {}",
                fixture.stage.join("askpass-exe").display(),
                exe.display()
            )),
            "the hidden binary was not bound back: {log}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_complete_script_hides_an_engine_tmpdir_that_is_not_under_tmp() {
        let fixture = fixture("confinement-separate-temp");
        // A path that is not under `/tmp`: the branch that mounts a second private
        // tmpfs over the engine's own TMPDIR. It is relative here because the test
        // host only lets the test write below its scratch root; the script itself
        // does not care whether the engine TMPDIR is absolute.
        let engine_temp = PathBuf::from("synthetic-engine-tmp");
        let script = unshare_script(&NamespacePlan {
            workspace: &fixture.workspace,
            hide: &fixture.hide,
            root: &fixture.run_root,
            stage: &fixture.stage,
            temp: &engine_temp,
            exe: Some(&fixture.exe),
        });
        let output = run_script(&fixture, &script);
        assert!(
            output.status.success(),
            "a separate engine TMPDIR could not be replaced: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&fixture.log).expect("the mount log");
        let temp_mount = log
            .lines()
            .position(|line| line == format!("-t tmpfs tmpfs {}", engine_temp.display()))
            .unwrap_or_else(|| panic!("the separate engine TMPDIR was not hidden: {log}"));
        let root_back = log
            .lines()
            .position(|line| {
                line == format!(
                    "--bind {} {}",
                    fixture.stage.join("root").display(),
                    fixture.run_root.display()
                )
            })
            .unwrap_or_else(|| panic!("the run root was not bound back: {log}"));
        assert!(
            temp_mount < root_back,
            "the run root is bound before its parent is private: {log}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_unshare_script_creates_every_bind_target_before_the_first_bind() {
        let fixture = fixture("confinement-script-targets");
        let script = unshare_script(&plan(&fixture, &fixture.temp, Some(&fixture.exe)));
        // The fake mount rejects a missing target, so a success proves the order.
        let output = run_script(&fixture, &script);
        assert!(
            output.status.success(),
            "a bind target was missing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = fs::read_to_string(&fixture.log).expect("the mount log");
        let first_bind = log
            .lines()
            .find(|line| line.starts_with("--bind"))
            .expect("a bind");
        assert!(
            first_bind.contains(&format!("{}", fixture.stage.join("workspace").display())),
            "the first bind is not the staged workspace: {log}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_unshare_script_leaves_its_private_tmp_as_a_binding_order() {
        let fixture = fixture("confinement-script-order");
        let script = unshare_script(&plan(&fixture, &fixture.temp, Some(&fixture.exe)));
        // With TMPDIR under `/tmp`, the `/tmp` tmpfs is what hides the sibling; the
        // run root still comes back after it.
        let temp_hide = script
            .find("mount -t tmpfs tmpfs /tmp")
            .expect("the private /tmp is not mounted");
        let root_back = script
            .find(&format!(
                "mount --bind {} {}",
                quote(&fixture.stage.join("root").display().to_string()),
                quote(&fixture.run_root.display().to_string())
            ))
            .expect("the run root is not bound back");
        assert!(
            temp_hide < root_back,
            "the run root is bound before the private TMPDIR hides the sibling:\n{script}"
        );
        let elsewhere = PathBuf::from("/var/tmp/gewu-engine-tmp");
        let namespace = plan(&fixture, &elsewhere, Some(&fixture.exe));
        let script = unshare_script(&namespace);
        let separate = script
            .find(&format!(
                "mount -t tmpfs tmpfs {}",
                quote(&elsewhere.display().to_string())
            ))
            .expect("a separate engine TMPDIR is not hidden");
        let root_back = script
            .find(&format!(
                "mount --bind {} {}",
                quote(&fixture.stage.join("root").display().to_string()),
                quote(&fixture.run_root.display().to_string())
            ))
            .expect("the run root is not bound back");
        assert!(
            separate < root_back,
            "a separate engine TMPDIR is hidden after the root is bound:\n{script}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn both_namespaces_keep_the_askpass_binary_after_the_hides() {
        let fixture = fixture("confinement-askpass");
        let namespace = plan(&fixture, &fixture.temp, Some(&fixture.exe));
        let text = bwrap_args(&namespace, "true").join(" ");
        let stage = text
            .find(&fixture.stage.join("askpass-exe").display().to_string())
            .expect("the binary is not staged");
        let hide = text.find("--tmpfs").expect("nothing is hidden");
        assert!(
            stage < hide,
            "the binary is staged after the first hide: {text}"
        );
        let script = unshare_script(&namespace);
        let stage = script
            .find(&fixture.stage.join("askpass-exe").display().to_string())
            .expect("the script does not stage the askpass binary");
        let hide = script.find("mount -t tmpfs").expect("nothing is hidden");
        assert!(
            stage < hide,
            "the script stages the askpass binary after its first hide:\n{script}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_unshare_script_makes_propagation_private_before_any_mount() {
        let fixture = fixture("confinement-script-propagate");
        let nested = fixture.hide[0].join("agents/agent-1/workspace/config");
        fs::create_dir_all(&nested).expect("the nested directory");
        let mut hide = fixture.hide.clone();
        hide.push(nested.clone());
        let namespace = NamespacePlan {
            workspace: &fixture.workspace,
            hide: &hide,
            root: &fixture.run_root,
            stage: &fixture.stage,
            temp: &fixture.temp,
            exe: Some(&fixture.exe),
        };
        let script = unshare_script(&namespace);
        let private = script
            .find("mount --make-rprivate /")
            .expect("the script does not make propagation private");
        let first_mount = script
            .find("mount --bind")
            .expect("the script mounts nothing");
        assert!(
            private < first_mount,
            "the script mounts before making propagation private:\n{script}"
        );
        // The nested path is mounted before its parent, so its name still exists.
        let nested_at = script
            .find(&nested.display().to_string())
            .expect("the nested path is not hidden");
        let home_at = script
            .find(&format!(
                "tmpfs {}\n",
                quote(&fixture.hide[0].display().to_string())
            ))
            .expect("the configuration root is not hidden");
        assert!(
            nested_at < home_at,
            "the parent was hidden before the nested path:\n{script}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_bwrap_arguments_make_tmp_private_and_never_bind_a_sibling() {
        let fixture = fixture("confinement-bwrap");
        let namespace = plan(&fixture, &fixture.temp, Some(&fixture.exe));
        let args = bwrap_args(&namespace, "true");
        let text = args.join(" ");
        assert!(text.contains("--tmpfs /tmp"), "{text}");
        if !fixture.temp.starts_with("/tmp") {
            assert!(
                text.contains(&format!("--tmpfs {}", fixture.temp.display())),
                "{text}"
            );
        }
        let tmpfs = args
            .iter()
            .position(|arg| arg == "--tmpfs")
            .expect("no tmpfs");
        let root_back = args
            .iter()
            .rposition(|arg| arg == &fixture.stage.join("root").display().to_string())
            .expect("the run root is not restored");
        assert!(
            tmpfs < root_back,
            "the root is bound before /tmp is private: {text}"
        );
        assert!(
            !text.contains(&fixture.sibling.display().to_string()),
            "a sibling run is named: {text}"
        );
        let elsewhere = PathBuf::from("/var/tmp/gewu-engine-tmp");
        let namespace = plan(&fixture, &elsewhere, Some(&fixture.exe));
        let text = bwrap_args(&namespace, "true").join(" ");
        assert!(
            text.contains(&format!("--tmpfs {}", elsewhere.display())),
            "a separate engine TMPDIR is not hidden: {text}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_bwrap_arguments_do_not_touch_a_binary_that_is_still_visible() {
        let fixture = fixture("confinement-bwrap-binary");
        // A system-wide binary is not under `/tmp`, the engine TMPDIR or a hidden
        // root, so no replacement mount is needed and nothing may truncate it.
        let exe = PathBuf::from("/usr/local/bin/gewu-multiverse");
        let namespace = plan(&fixture, &fixture.temp, Some(&exe));
        let text = bwrap_args(&namespace, "true").join(" ");
        assert!(
            !text.contains(&format!("--file {}", exe.display())),
            "a visible binary gets a replacement mount: {text}"
        );
        let script = unshare_script(&namespace);
        assert!(
            !script.contains(&format!(": > {}", exe.display())),
            "a visible binary is truncated: {script}"
        );
        let _ = fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn the_opt_in_parser_names_only_the_two_namespace_words() {
        assert_eq!(requested_namespace_from(None), None);
        assert_eq!(requested_namespace_from(Some("")), None);
        assert_eq!(requested_namespace_from(Some("landlock")), None);
        assert_eq!(requested_namespace_from(Some("bwrap")), Some(Method::Bwrap));
        assert_eq!(
            requested_namespace_from(Some("unshare")),
            Some(Method::Unshare)
        );
    }

    #[test]
    fn every_existing_workspace_is_enumerated_and_a_symlinked_one_is_unconfined() {
        // B9-R-R: the engine used to hand `detect` only `<config>/agents`, so a
        // workspace symlinked over the configuration parent was invisible. The
        // enumerator must see it, and the canonical overlap must refuse the layout.
        let root = scratch("confinement-workspaces");
        let config = root.join("config");
        let data = root.join("data");
        let normal = config.join("agents/agent-1/workspace");
        let symlinked = config.join("agents/agent-2/workspace");
        for path in [&normal, &data, symlinked.parent().expect("a parent")] {
            fs::create_dir_all(path).expect("a fixture directory");
        }
        std::os::unix::fs::symlink(&root, &symlinked).expect("the workspace symlink");
        let workspaces = agent_workspaces(&config.join("agents"));
        assert_eq!(
            workspaces,
            vec![normal, symlinked],
            "a workspace was missed"
        );
        assert_eq!(
            detect(&config, &root.join("state"), &data, &workspaces),
            Method::None,
            "detect claimed a boundary for a workspace symlinked over the config parent"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A writable directory outside `/tmp` and the engine TMPDIR, beside the test
    /// binary. `None` when this build's target is itself under `/tmp`: the caller
    /// then skips the case rather than asserting a location it does not have.
    fn outside_temp(tag: &str) -> Option<PathBuf> {
        let parent = std::env::current_exe().ok()?.parent()?.to_path_buf();
        if parent.starts_with("/tmp") || parent.starts_with(std::env::temp_dir()) {
            return None;
        }
        let dir = parent.join(format!(
            "gewu-confinement-unit-{}-{tag}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    #[test]
    fn a_hide_under_tmp_whose_target_is_outside_is_resolved_for_the_namespace() {
        // B5-R-R-R: a hide name under the engine TMPDIR is skipped because the
        // private `/tmp` is assumed to cover it. If it is a symlink to a directory
        // outside `/tmp`, its target is not covered at all. Resolution must put the
        // target in the hide list and never hand bwrap the symlink.
        let Some(outside) = outside_temp("hide-target") else {
            eprintln!("no writable place outside /tmp for this test's target; skipped");
            return;
        };
        let temp = std::env::temp_dir();
        let target = outside.join("real-state");
        fs::create_dir_all(&target).expect("the state target");
        let link = temp.join(format!(
            "gewu-hide-link-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        std::os::unix::fs::symlink(&target, &link).expect("the state symlink");
        let resolved =
            resolved_hide_list(std::slice::from_ref(&link)).expect("a resolvable hide is resolved");
        assert_eq!(resolved, vec![target.clone()]);
        assert!(!resolved.contains(&link));
        let workspace = outside.join("workspace");
        let stage = outside.join("stage");
        let root = outside.join("run-1");
        for path in [&workspace, &stage, &root] {
            fs::create_dir_all(path).expect("a fixture directory");
        }
        let args = bwrap_args(
            &NamespacePlan {
                workspace: &workspace,
                hide: &resolved,
                root: &root,
                stage: &stage,
                temp: &temp,
                exe: None,
            },
            "true",
        )
        .join(" ");
        assert!(
            args.contains(&format!("--tmpfs {}", target.display())),
            "the target outside /tmp was not hidden: {args}"
        );
        assert!(
            !args.contains(&format!("--tmpfs {}", link.display())),
            "the symlink itself reached bwrap: {args}"
        );
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_file(&link);
    }

    #[test]
    fn a_dangling_hide_cannot_be_placed_and_so_it_is_never_called_confined() {
        let root = scratch("confinement-dangling-hide");
        let dangling = root.join("state-link");
        std::os::unix::fs::symlink(root.join("missing-target"), &dangling).expect("the link");
        assert!(
            resolved_hide_list(std::slice::from_ref(&dangling)).is_none(),
            "a dangling hide was resolved lexically"
        );
        let (method, reason) = effective_method(
            Method::Bwrap,
            std::slice::from_ref(&dangling),
            &root.join("workspace"),
        );
        assert_eq!(method, Method::None);
        assert!(reason.is_some(), "the refusal has no reason");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_binary_under_the_private_tmp_is_restored_after_the_hides() {
        // N16's other branch: when the build's target is under `/tmp` (or the engine
        // TMPDIR), the private tmpfs hides the product binary too, so bwrap must get
        // the restore mount. The test computes the branch from the path instead of
        // assuming where cargo built it.
        let fixture = fixture("confinement-binary-under-tmp");
        let exe = std::env::temp_dir().join(format!(
            "gewu-product-under-tmp-{}-{}",
            std::process::id(),
            crate::time::now_unix_ms()
        ));
        fs::write(&exe, b"#!/bin/sh\nexit 0\n").expect("the binary");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).expect("the mode");
        let namespace = plan(&fixture, &fixture.temp, Some(&exe));
        let text = bwrap_args(&namespace, "true").join(" ");
        assert!(
            text.contains(&format!(
                "--ro-bind {} {}",
                fixture.stage.join("askpass-exe").display(),
                exe.display()
            )),
            "a binary under the private /tmp was not restored: {text}"
        );
        let _ = fs::remove_file(&exe);
        let _ = fs::remove_dir_all(&fixture.root);
    }
}
