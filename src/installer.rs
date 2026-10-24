//! The installer engine: one exclusive publication at a time, cancellable stages,
//! and a durable journal so a killed install never costs the usable old version.
//!
//! Everything here works inside the product-owned `harnesses/<id>/` directory. A
//! stage or a backup is deleted only after its own owner marker (or the exact prior
//! install record recorded in the journal) proves it belongs to this installer; a
//! foreign directory, a symlink or an unrecognised lock is left untouched.

use nix::fcntl::{Flock, FlockArg};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

use crate::harness::{self, InstallRecord, InstalledProgram, Program};

/// The sentence every cancellation path returns. The callers show it as a normal
/// outcome, not as an install failure with a stale error card.
pub const INSTALL_CANCELED: &str =
    "the installation was canceled; the previous version is unchanged";

const FORMAT: u32 = 1;
const LOCK_FILE: &str = ".install.lock";
const JOURNAL_FILE: &str = ".install-journal";
const OWNER_FILE: &str = ".gewu-install-owner.json";
const STAGE_PREFIX: &str = ".stage-";
const OLD_PREFIX: &str = ".old-";
const CHILD_POLL: Duration = Duration::from_millis(40);

/// Which part of the install the caller is watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Preparing,
    Downloading,
    Unpacking,
    InstallingPackages,
    Verifying,
}

impl InstallPhase {
    /// One short word for the board, not an implementation name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Downloading => "downloading",
            Self::Unpacking => "unpacking",
            Self::InstallingPackages => "installing packages",
            Self::Verifying => "checking",
        }
    }
}

/// One truthful progress sample. `percent` is present only when the server declared
/// a length and bytes arrived; a phase without a number is never drawn as a stale
/// percentage from the previous phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub phase: InstallPhase,
    pub percent: Option<u8>,
    pub received: u64,
    pub total: Option<u64>,
}

impl InstallProgress {
    fn phase(phase: InstallPhase) -> Self {
        Self {
            phase,
            percent: None,
            received: 0,
            total: None,
        }
    }
}

/// A flag shared by the caller (a key, a quit, the CLI's signal handler) and the
/// download, `tar` and npm steps.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the running install to stop at its next safe point.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// The same flag, for `signal_hook` or a test.
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

/// Install one pinned program, or return the already-installed pinned one.
pub fn install(
    data_root: &Path,
    program: Program,
    cancel: &CancelToken,
    mut progress: impl FnMut(InstallProgress),
) -> Result<InstalledProgram, String> {
    if !program.is_native() {
        return Err("the built-in engine needs no install".to_owned());
    }
    let root = harness::install_root(data_root, program);
    let Some(directory) = root.parent().map(Path::to_path_buf) else {
        return Err("the install directory cannot be found".to_owned());
    };
    fs::create_dir_all(&directory)
        .map_err(|error| format!("the install directory cannot be created: {error}"))?;
    let directory_metadata = fs::symlink_metadata(&directory)
        .map_err(|error| format!("the install directory cannot be read: {error}"))?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(format!(
            "the {} install directory is not a plain directory; it was left untouched",
            program.label()
        ));
    }
    let _ = crate::mode::set_mode(&directory, crate::mode::PRIVATE_DIR_MODE);
    let _lock = InstallLock::acquire(&directory, program)?;
    // A killed predecessor is repaired before this attempt reads or replaces the
    // version root. The lock means no other installer is in this directory.
    recover_locked(&directory, &root, program)?;
    if let Some(existing) = harness::read_installed_at_root(&root, program)
        && existing.version == program.pinned_version()
    {
        return Ok(existing);
    }
    if let Some(prerequisite) = program.prerequisite_missing() {
        return Err(prerequisite.to_owned());
    }
    let prior_record_sha256 = if root.exists() {
        if fs::symlink_metadata(&root)
            .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
            .unwrap_or(true)
        {
            return Err(format!(
                "the existing {} directory is not a plain install directory; it was left untouched",
                program.label()
            ));
        }
        Some(sha256_file(&root.join("install.json")).map_err(|_| {
            format!(
                "the existing {} install cannot be identified; it was left untouched",
                program.label()
            )
        })?)
    } else {
        None
    };
    check_cancel(cancel)?;
    progress(InstallProgress::phase(InstallPhase::Preparing));

    let token = new_token();
    let stage = directory.join(format!("{STAGE_PREFIX}{token}"));
    if stage.exists() {
        return Err(format!(
            "{} already has a directory named {}; it was left untouched",
            program.label(),
            stage.display()
        ));
    }
    fs::create_dir(&stage)
        .map_err(|error| format!("the install stage cannot be created: {error}"))?;
    let _ = crate::mode::set_mode(&stage, crate::mode::PRIVATE_DIR_MODE);
    if let Err(error) = write_owner(&stage, program, &token) {
        // The marker could not be written, so this empty directory is not yet an
        // owned stage. `remove_dir` refuses a non-empty directory, which is exactly
        // the safe bound here.
        let _ = fs::remove_dir(&stage);
        return Err(error);
    }

    let built = build_stage(program, &stage, cancel, &mut progress);
    if let Err(error) = built {
        let _ = remove_owned_stage(&stage, program, &token);
        return Err(error);
    }
    if harness::read_installed_at_root(&stage, program).is_none() {
        let _ = remove_owned_stage(&stage, program, &token);
        return Err(format!(
            "{} was unpacked but its program or required helper cannot be read back",
            program.label()
        ));
    }
    progress(InstallProgress::phase(InstallPhase::Verifying));
    check_cancel(cancel)?;

    let journal = Journal {
        format: FORMAT,
        program: program.id().to_owned(),
        version: program.pinned_version().to_owned(),
        token: token.clone(),
        stage: file_name(&stage)?,
        old: Some(format!("{OLD_PREFIX}{token}")),
        state: JournalState::Staged,
        had_old: root.exists(),
        prior_record_sha256,
    };
    let journal_path = directory.join(JOURNAL_FILE);
    write_journal(&journal_path, &journal)?;
    let result = publish(program, &root, &directory, journal, cancel);
    if result.is_err() {
        // A publish failure before the rename restores the old root itself. If it
        // could not, the stage stays and the journal records how to recover it.
        let _ = remove_owned_stage(&stage, program, &token);
    }
    result
}

/// Repair interrupted installs under every native program. Called once when the
/// engine opens, so a kill between the two renames is restored before the first
/// picture says the program is missing.
pub fn recover_pending(data_root: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for program in harness::NATIVE_PROGRAMS {
        let root = harness::install_root(data_root, program);
        let Some(directory) = root.parent().map(Path::to_path_buf) else {
            continue;
        };
        if fs::symlink_metadata(&directory)
            .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
            .unwrap_or(true)
        {
            continue;
        }
        // A live install (or a lock this installer did not write) owns the
        // directory. It is not a recovery failure here.
        if let Ok(_lock) = InstallLock::acquire(&directory, program)
            && let Err(error) = recover_locked(&directory, &root, program)
        {
            notes.push(format!(
                "the interrupted {} install could not be recovered: {error}",
                program.label()
            ));
        }
    }
    notes
}

/// What one install wrote; used by the recovery checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    format: u32,
    program: String,
    version: String,
    token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum JournalState {
    Staged,
    OldMoved,
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u32,
    program: String,
    version: String,
    token: String,
    stage: String,
    old: Option<String>,
    state: JournalState,
    had_old: bool,
    prior_record_sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockIdentity {
    format: u32,
    program: String,
    pid: u32,
    token: String,
    started_at: String,
}

/// One open lock descriptor. The kernel releases the advisory lock if this process
/// dies, so the file itself is kept (and never mistaken for a stale stage).
struct InstallLock {
    _lock: Flock<File>,
}

impl InstallLock {
    fn acquire(directory: &Path, program: Program) -> Result<Self, String> {
        let path = directory.join(LOCK_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "{} is not the installer's own lock; it was left untouched",
                    path.display()
                ));
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(&path)
            .map_err(|error| format!("{} cannot be opened: {error}", path.display()))?;
        let mut file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => lock,
            Err((_file, nix::errno::Errno::EWOULDBLOCK)) => {
                return Err(format!(
                    "{} is already being installed by another process",
                    program.label()
                ));
            }
            Err((_file, error)) => {
                return Err(format!("{} cannot be locked: {error}", path.display()));
            }
        };
        let mut current = String::new();
        file.read_to_string(&mut current)
            .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
        if !current.trim().is_empty() {
            let previous: LockIdentity = serde_json::from_str(&current).map_err(|_| {
                format!(
                    "{} contains something this installer did not write; it was left untouched",
                    path.display()
                )
            })?;
            if previous.format != FORMAT || previous.program != program.id() {
                return Err(format!(
                    "{} belongs to something else; it was left untouched",
                    path.display()
                ));
            }
        }
        let identity = LockIdentity {
            format: FORMAT,
            program: program.id().to_owned(),
            pid: std::process::id(),
            token: new_token(),
            started_at: harness::now(),
        };
        file.set_len(0)
            .map_err(|error| format!("{} cannot be prepared: {error}", path.display()))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("{} cannot be prepared: {error}", path.display()))?;
        file.write_all(
            serde_json::to_string(&identity)
                .map_err(|error| format!("the install lock cannot be written: {error}"))?
                .as_bytes(),
        )
        .map_err(|error| format!("{} cannot be written: {error}", path.display()))?;
        file.sync_all()
            .map_err(|error| format!("{} cannot be synced: {error}", path.display()))?;
        Ok(Self { _lock: file })
    }
}

fn build_stage(
    program: Program,
    stage: &Path,
    cancel: &CancelToken,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    match program {
        Program::Pi => archive_install(
            program,
            stage,
            "pi.tar.gz",
            "pi/pi",
            "the Pi package did not contain its executable",
            cancel,
            progress,
        ),
        Program::Codex => archive_install(
            program,
            stage,
            "codex.tar.gz",
            "bin/codex",
            "the Codex package did not contain its executable",
            cancel,
            progress,
        ),
        Program::Opencode => archive_install(
            program,
            stage,
            "opencode.tgz",
            "package/bin/opencode",
            "the OpenCode package did not contain its executable",
            cancel,
            progress,
        ),
        Program::Dsh => install_dsh(stage, cancel, progress),
        Program::Builtin => unreachable!(),
    }
}

fn archive_install(
    program: Program,
    stage: &Path,
    archive_name: &str,
    entrypoint: &str,
    missing: &str,
    cancel: &CancelToken,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    let artifact = harness::artifact(program)
        .ok_or_else(|| format!("{} has no official package address", program.label()))?;
    let archive = stage.join(archive_name);
    progress(InstallProgress::phase(InstallPhase::Downloading));
    download(&artifact.url, &archive, &artifact.sha256, cancel, progress)?;
    check_cancel(cancel)?;
    progress(InstallProgress::phase(InstallPhase::Unpacking));
    extract(&archive, stage, cancel)?;
    let executable = stage.join(entrypoint);
    let metadata = fs::metadata(&executable).map_err(|_| format!("{missing} at {entrypoint}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!("{missing} as an executable at {entrypoint}"));
    }
    write_record(stage, program, entrypoint, None)?;
    Ok(())
}

fn write_record(
    stage: &Path,
    program: Program,
    entrypoint: &str,
    resources: Option<String>,
) -> Result<(), String> {
    let artifact = harness::artifact(program)
        .ok_or_else(|| format!("{} has no official package address", program.label()))?;
    let record = InstallRecord {
        program: program.id().to_owned(),
        version: program.pinned_version().to_owned(),
        sha256: artifact.sha256,
        license: program.license().to_owned(),
        entrypoint: entrypoint.to_owned(),
        resources,
        installed_at: harness::now(),
    };
    let record_path = harness::record_path(stage);
    if let Ok(metadata) = fs::symlink_metadata(&record_path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(
            "the package left something at its install record path; it was not followed".to_owned(),
        );
    }
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| format!("the install record cannot be written: {error}"))?;
    crate::mode::write_private(record_path, text.as_bytes())
        .map_err(|error| format!("the install record cannot be written: {error}"))
}

fn install_dsh(
    stage: &Path,
    cancel: &CancelToken,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    progress(InstallProgress::phase(InstallPhase::InstallingPackages));
    verify_dsh_script_policy()?;
    let package = stage.join("package.json");
    let lock = stage.join("package-lock.json");
    crate::mode::write_private(&package, include_bytes!("../vendor/dsh/package.json"))
        .map_err(|error| format!("the DeepSeek Harness package file cannot be written: {error}"))?;
    crate::mode::write_private(&lock, include_bytes!("../vendor/dsh/package-lock.json"))
        .map_err(|error| format!("the DeepSeek Harness lockfile cannot be written: {error}"))?;
    let home = stage.join("npm-home");
    let cache = stage.join("npm-cache");
    fs::create_dir_all(&home)
        .map_err(|error| format!("the npm home cannot be created: {error}"))?;
    fs::create_dir_all(&cache)
        .map_err(|error| format!("the npm cache cannot be created: {error}"))?;
    let _ = crate::mode::set_mode(&home, crate::mode::PRIVATE_DIR_MODE);
    let _ = crate::mode::set_mode(&cache, crate::mode::PRIVATE_DIR_MODE);
    let log_path = stage.join("npm-install.log");
    check_npm_scripts_enabled(stage, &home, &cache)?;
    check_cancel(cancel)?;
    let mut command = Command::new("npm");
    command
        .args(["ci", "--no-audit", "--no-fund", "--no-progress", "--prefix"])
        .arg(stage)
        .env("HOME", &home)
        .env("npm_config_cache", &cache)
        .env("npm_config_registry", "https://registry.npmjs.org/")
        .env("npm_config_update_notifier", "false")
        .env("npm_config_audit", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_progress", "false")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH");
    let status = run_child(command, cancel, &log_path).map_err(|error| {
        if error == INSTALL_CANCELED {
            return error;
        }
        format!("npm could not install DeepSeek Harness from its locked packages ({error})")
    })?;
    if !status.success() {
        return Err(npm_failure(&log_path));
    }
    check_cancel(cancel)?;

    let package_json = stage.join("node_modules/@deepseek-ai/dsh/package.json");
    let text = fs::read_to_string(&package_json).map_err(|error| {
        format!("the installed DeepSeek Harness record cannot be read: {error}")
    })?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the installed DeepSeek Harness record is not valid: {error}"))?;
    if value.get("version").and_then(serde_json::Value::as_str)
        != Some(Program::Dsh.pinned_version())
    {
        return Err(format!(
            "npm installed a different DeepSeek Harness version than {}",
            Program::Dsh.pinned_version()
        ));
    }
    let entrypoint = "node_modules/.bin/dsh";
    let executable = stage.join(entrypoint);
    let metadata = fs::metadata(&executable)
        .map_err(|_| "the DeepSeek Harness package has no runnable entrypoint".to_owned())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("the DeepSeek Harness entrypoint is not executable".to_owned());
    }
    verify_dsh_helpers(stage)?;
    verify_dsh_package_artifact(stage, cancel)?;

    let lock_sha = hex(&Sha256::digest(include_bytes!(
        "../vendor/dsh/package-lock.json"
    )));
    let pty = dsh_pty_path(stage)
        .ok_or_else(|| "the DeepSeek Harness terminal helper is not installed".to_owned())?;
    let pty_sha = sha256_file(&pty)?;
    let koffi_version = json_field(&stage.join("node_modules/koffi/package.json"), "version")
        .unwrap_or_else(|| "unknown".to_owned());
    let resources = format!(
        "npm lock sha256 {lock_sha}; @deepseek-ai/dsh tarball sha256 {} (verified; lock integrity sha512 verified); entrypoint {entrypoint}; node-pty pty.node sha256 {pty_sha}; koffi {koffi_version}; install scripts reviewed: node-pty, koffi and dsh-subprocess-local; no-op scripts denied: @google/genai and protobufjs; no recursive tree digest is claimed",
        harness::artefact_sha256(Program::Dsh)
    );
    // The npm home and cache are installer state, not part of the program.
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&cache);
    let _ = fs::remove_file(&log_path);
    write_record(stage, Program::Dsh, entrypoint, Some(resources))
}

fn check_npm_scripts_enabled(stage: &Path, home: &Path, cache: &Path) -> Result<(), String> {
    let output = Command::new("npm")
        .args(["config", "get", "ignore-scripts"])
        .current_dir(stage)
        .env("HOME", home)
        .env("npm_config_cache", cache)
        .env("npm_config_registry", "https://registry.npmjs.org/")
        .env("npm_config_update_notifier", "false")
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("npm could not be started: {error}"))?;
    if !output.status.success() {
        return Err("npm could not read its install-script setting".to_owned());
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value == "true" {
        return Err(
            "npm skips install scripts; DeepSeek Harness needs the scripts reviewed by this installer"
                .to_owned(),
        );
    }
    Ok(())
}

/// The exact install-script set in the locked graph, reviewed by this product. A
/// changed lock is not silently run: it fails before npm starts until the new
/// scripts are reviewed.
fn verify_dsh_script_policy() -> Result<(), String> {
    const REVIEWED: [(&str, &str, bool); 5] = [
        ("@deepseek-ai/dsh-subprocess-local", "0.2.1-rc.2", true),
        ("koffi", "3.1.1", true),
        ("node-pty", "1.2.0-beta.15", true),
        ("@google/genai", "2.21.0", false),
        ("protobufjs", "7.6.6", false),
    ];
    let package: serde_json::Value =
        serde_json::from_slice(include_bytes!("../vendor/dsh/package.json"))
            .map_err(|error| format!("the DeepSeek Harness package file is not valid: {error}"))?;
    let policy = package
        .get("allowScripts")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            "the DeepSeek Harness package does not review its install scripts".to_owned()
        })?;
    let lock: serde_json::Value =
        serde_json::from_slice(include_bytes!("../vendor/dsh/package-lock.json"))
            .map_err(|error| format!("the DeepSeek Harness lockfile is not valid: {error}"))?;
    let packages = lock
        .get("packages")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "the DeepSeek Harness lockfile has no package graph".to_owned())?;
    let mut scripts = Vec::new();
    for (path, value) in packages {
        if path.is_empty() || value.get("hasInstallScript") != Some(&serde_json::Value::Bool(true))
        {
            continue;
        }
        let name = package_name_from_lock_path(path).ok_or_else(|| {
            format!("the DeepSeek Harness lockfile names an unclear package: {path}")
        })?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("the DeepSeek Harness lockfile has no version for {name}"))?;
        scripts.push((name.to_owned(), version.to_owned()));
    }
    scripts.sort();
    let mut reviewed = REVIEWED
        .iter()
        .map(|(name, version, allowed)| ((*name).to_owned(), (*version).to_owned(), *allowed))
        .collect::<Vec<_>>();
    reviewed.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    if scripts.len() != reviewed.len()
        || scripts.iter().zip(&reviewed).any(
            |((name, version), (expected, expected_version, _))| {
                name != expected || version != expected_version
            },
        )
    {
        return Err(
            "the locked DeepSeek Harness packages changed their install scripts; this installer has not reviewed the new set, so nothing was run"
                .to_owned(),
        );
    }
    for (name, version, allowed) in reviewed {
        let key = format!("{name}@{version}");
        if policy.get(&key).and_then(serde_json::Value::as_bool) != Some(allowed) {
            return Err(format!(
                "the DeepSeek Harness install-script review for {key} is missing or different; nothing was run"
            ));
        }
    }
    if policy.len() != REVIEWED.len() {
        return Err(
            "the DeepSeek Harness install-script review names a package outside the locked graph; nothing was run"
                .to_owned(),
        );
    }
    Ok(())
}

fn package_name_from_lock_path(path: &str) -> Option<&str> {
    let marker = "node_modules/";
    let rest = path.rsplit(marker).next()?;
    if rest.is_empty() || rest.contains("node_modules/") {
        return None;
    }
    Some(rest)
}

fn verify_dsh_helpers(stage: &Path) -> Result<(), String> {
    let pty = dsh_pty_path(stage)
        .ok_or_else(|| "terminal helper missing; the old version is still installed".to_owned())?;
    if !pty.is_file() {
        return Err("the DeepSeek Harness terminal helper is missing".to_owned());
    }
    for helper in dsh_spawn_helpers(stage) {
        if helper.is_file()
            && fs::metadata(&helper)
                .map(|metadata| metadata.permissions().mode() & 0o111 == 0)
                .unwrap_or(true)
        {
            return Err(
                "required install script skipped; the old version is still installed".to_owned(),
            );
        }
    }
    let script = "const pty = require('node-pty'); if (typeof pty.spawn !== 'function') throw new Error('node-pty helper'); const koffi = require('koffi'); const pid = koffi.load(null).func('int getpid()')(); if (!Number.isInteger(pid) || pid <= 0) throw new Error('koffi helper'); process.stdout.write('ok');";
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .current_dir(stage)
        .env("HOME", stage.join("npm-home"))
        .env("TMPDIR", stage)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("the DeepSeek Harness helper probe could not start: {error}"))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != "ok" {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.lines().next().unwrap_or("").trim();
        return Err(if detail.is_empty() {
            "helper did not run; the old version is still installed".to_owned()
        } else {
            format!("helper did not run; the old version is still installed ({detail})")
        });
    }
    Ok(())
}

/// Verify the declared top-level package tarball as well as the lock's sha512. The
/// rest of the graph was verified by npm against the lock's integrity values.
fn verify_dsh_package_artifact(stage: &Path, cancel: &CancelToken) -> Result<(), String> {
    let artifact = harness::artifact(Program::Dsh)
        .ok_or_else(|| "DeepSeek Harness has no official package address".to_owned())?;
    let path = stage.join(".dsh-package.tgz");
    if let Err(error) = download(&artifact.url, &path, &artifact.sha256, cancel, &mut |_| {}) {
        return Err(if error == INSTALL_CANCELED {
            error
        } else {
            format!("the official DeepSeek Harness package could not be verified: {error}")
        });
    }
    let bytes = fs::read(&path).map_err(|error| {
        format!("the official DeepSeek Harness package cannot be read: {error}")
    })?;
    let integrity = format!("sha512-{}", base64(&Sha512::digest(&bytes)));
    if integrity != dsh_lock_integrity() {
        let _ = fs::remove_file(&path);
        return Err("package checksum mismatch; the old version is still installed".to_owned());
    }
    let _ = fs::remove_file(&path);
    Ok(())
}

fn dsh_lock_integrity() -> &'static str {
    "sha512-tvY+yhfKRifthbqPe/OK0AYxM5ZkvqfYq/jMYgJcEUxiRRL1Vwra3Ti5TB6AXgSWQdiPHYh/Vp6SJa9yHwMNuw=="
}

fn node_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

fn dsh_pty_path(root: &Path) -> Option<PathBuf> {
    let platform = node_platform();
    [
        root.join(format!(
            "node_modules/node-pty/prebuilds/{platform}/pty.node"
        )),
        root.join("node_modules/node-pty/build/Release/pty.node"),
        root.join("node_modules/node-pty/build/Debug/pty.node"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn dsh_spawn_helpers(root: &Path) -> Vec<PathBuf> {
    let platform = node_platform();
    let mut helpers = vec![
        root.join(format!(
            "node_modules/node-pty/prebuilds/{platform}/spawn-helper"
        )),
        root.join("node_modules/node-pty/build/Release/spawn-helper"),
    ];
    helpers.retain(|path| path.exists());
    helpers
}

fn download(
    url: &str,
    target: &Path,
    expected_sha256: &str,
    cancel: &CancelToken,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<(), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|error| format!("the official package could not be reached ({error})"))?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        return Err(format!("the official package answered HTTP {status}"));
    }
    let total = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let stage = target.with_extension("part");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(crate::mode::SECRET_FILE_MODE)
        .open(&stage)
        .map_err(|error| format!("the package cannot be written: {error}"))?;
    let mut reader = response.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(_) if cancel.is_cancelled() => {
                let _ = fs::remove_file(&stage);
                return Err(INSTALL_CANCELED.to_owned());
            }
            Err(error) => {
                return Err(format!(
                    "the official package stopped while downloading: {error}"
                ));
            }
        };
        if cancel.is_cancelled() {
            let _ = fs::remove_file(&stage);
            return Err(INSTALL_CANCELED.to_owned());
        }
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("the package cannot be written: {error}"))?;
        received += read as u64;
        progress(InstallProgress {
            phase: InstallPhase::Downloading,
            percent: total
                .filter(|total| *total > 0)
                .map(|total| ((received.saturating_mul(100) / total).min(100)) as u8),
            received,
            total,
        });
    }
    file.sync_all()
        .map_err(|error| format!("the package cannot be synced: {error}"))?;
    drop(file);
    if cancel.is_cancelled() {
        let _ = fs::remove_file(&stage);
        return Err(INSTALL_CANCELED.to_owned());
    }
    let actual = hex(&hasher.finalize());
    if actual != expected_sha256 {
        let _ = fs::remove_file(&stage);
        return Err("package checksum mismatch; the old version is still installed".to_owned());
    }
    fs::rename(&stage, target)
        .map_err(|error| format!("the package cannot be put in place: {error}"))
}

fn extract(archive: &Path, target: &Path, cancel: &CancelToken) -> Result<(), String> {
    let mut command = Command::new("tar");
    command.arg("-xzf").arg(archive).arg("-C").arg(target);
    let log = target.join("archive.log");
    let status = run_child(command, cancel, &log).map_err(|error| {
        if error == INSTALL_CANCELED {
            error
        } else {
            "the official package could not be unpacked; the old version is still installed"
                .to_owned()
        }
    })?;
    if !status.success() {
        return Err("invalid package archive; the old version is still installed".to_owned());
    }
    let _ = fs::remove_file(&log);
    let _ = fs::remove_file(archive);
    Ok(())
}

fn run_child(
    mut command: Command,
    cancel: &CancelToken,
    log: &Path,
) -> Result<std::process::ExitStatus, String> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(crate::mode::SECRET_FILE_MODE)
        .open(log)
        .map_err(|error| format!("the install log cannot be written: {error}"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(file));
    // A killed installer must not leave npm or tar's own children behind.
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("the install command could not be started: {error}"))?;
    loop {
        if cancel.is_cancelled() {
            terminate(&mut child);
            return Err(INSTALL_CANCELED.to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(CHILD_POLL),
            Err(error) => {
                terminate(&mut child);
                return Err(format!(
                    "the install command could not be followed: {error}"
                ));
            }
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = child.kill();
    let _ = child.wait();
}

fn check_cancel(cancel: &CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err(INSTALL_CANCELED.to_owned())
    } else {
        Ok(())
    }
}

fn npm_failure(log: &Path) -> String {
    let tail = read_log_tail(log).unwrap_or_default();
    if tail.contains("not yet covered by allowScripts") {
        return "npm reported an install script outside the reviewed DeepSeek Harness set; nothing further was run"
            .to_owned();
    }
    if tail.is_empty() {
        "npm could not install DeepSeek Harness from its locked packages".to_owned()
    } else {
        format!("npm could not install DeepSeek Harness from its locked packages: {tail}")
    }
}

fn read_log_tail(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("npm warn allow-scripts"))
        .unwrap_or("");
    if line.is_empty() {
        None
    } else {
        Some(line.chars().take(240).collect())
    }
}

fn publish(
    program: Program,
    root: &Path,
    directory: &Path,
    mut journal: Journal,
    cancel: &CancelToken,
) -> Result<InstalledProgram, String> {
    let stage = directory.join(&journal.stage);
    let journal_path = directory.join(JOURNAL_FILE);
    let token = journal.token.clone();
    if cancel.is_cancelled() {
        let _ = fs::remove_file(&journal_path);
        return Err(INSTALL_CANCELED.to_owned());
    }
    let old = journal
        .old
        .as_ref()
        .map(|name| directory.join(name))
        .filter(|path| path.exists());
    if let Some(old) = &old {
        let _ = fs::remove_file(&journal_path);
        return Err(format!(
            "the backup directory {} already exists; it was left untouched",
            old.display()
        ));
    }
    if journal.had_old {
        let old_path = directory.join(
            journal
                .old
                .as_deref()
                .ok_or_else(|| "the install journal has no backup name".to_owned())?,
        );
        if let Err(error) = fs::rename(root, &old_path) {
            let _ = fs::remove_file(&journal_path);
            return Err(format!(
                "the old {} install cannot be moved aside ({error}); it is unchanged",
                program.label()
            ));
        }
        sync_dir(directory)?;
    }
    journal.state = JournalState::OldMoved;
    write_journal(&journal_path, &journal)?;
    maybe_pause("after-old-rename");
    if cancel.is_cancelled() {
        rollback(root, &stage, directory, &journal_path, &journal);
        return Err(INSTALL_CANCELED.to_owned());
    }
    if let Err(error) = fs::rename(&stage, root) {
        rollback(root, &stage, directory, &journal_path, &journal);
        return Err(format!(
            "{} cannot be put in place ({error}); the old version is unchanged",
            program.label()
        ));
    }
    sync_dir(directory)?;
    journal.state = JournalState::Published;
    write_journal(&journal_path, &journal)?;
    let installed = match harness::read_installed_at_root(root, program) {
        Some(installed) => installed,
        None => {
            rollback(root, &stage, directory, &journal_path, &journal);
            return Err(format!(
                "{} was put in place but its program or required helper cannot be read back; the old version is unchanged",
                program.label()
            ));
        }
    };
    maybe_pause("after-publish");
    if let Some(old_name) = &journal.old {
        let old_path = directory.join(old_name);
        if old_path.exists()
            && owned_old(
                &old_path,
                program,
                &token,
                journal.prior_record_sha256.as_deref(),
            )
        {
            let _ = fs::remove_dir_all(&old_path);
        }
    }
    // The new root is verified; a backup this installer cannot prove it owns was
    // preserved above and is not needed for recovery.
    let _ = fs::remove_file(&journal_path);
    sync_dir(directory)?;
    Ok(installed)
}

fn rollback(root: &Path, stage: &Path, directory: &Path, journal_path: &Path, journal: &Journal) {
    if root.exists() && !stage.exists() && root_owned_by(root, &journal.token) {
        let _ = fs::rename(root, stage);
    }
    if let Some(old_name) = &journal.old {
        let old_path = directory.join(old_name);
        if old_path.exists() && !root.exists() {
            let _ = fs::rename(&old_path, root);
        }
    }
    let _ = fs::remove_file(journal_path);
    let _ = sync_dir(directory);
}

fn recover_locked(directory: &Path, root: &Path, program: Program) -> Result<(), String> {
    let journal_path = directory.join(JOURNAL_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&journal_path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "{} is not the installer's own journal; it was left untouched",
            journal_path.display()
        ));
    }
    if let Ok(text) = fs::read_to_string(&journal_path) {
        let journal: Journal = serde_json::from_str(&text).map_err(|_| {
            format!(
                "{} is not a journal this installer wrote; it was left untouched",
                journal_path.display()
            )
        })?;
        if journal.format != FORMAT
            || journal.program != program.id()
            || journal.version != program.pinned_version()
        {
            return Err(format!(
                "{} belongs to another install; it was left untouched",
                journal_path.display()
            ));
        }
        let stage = directory.join(&journal.stage);
        validate_owned_name(&journal.stage, STAGE_PREFIX)?;
        let old = journal
            .old
            .as_ref()
            .map(|name| {
                validate_owned_name(name, OLD_PREFIX)?;
                Ok::<PathBuf, String>(directory.join(name))
            })
            .transpose()?;
        match journal.state {
            JournalState::Staged => {
                if !root.exists() {
                    // The old root may have been moved even though the journal was
                    // written before the first rename (a crash between them).
                    if let Some(old) = old.as_ref()
                        && old.exists()
                        && owned_old(
                            old,
                            program,
                            &journal.token,
                            journal.prior_record_sha256.as_deref(),
                        )
                    {
                        fs::rename(old, root).map_err(|error| {
                            format!("the previous install could not be restored: {error}")
                        })?;
                        sync_dir(directory)?;
                    } else if stage.exists() && stage_owned_by(&stage, &journal.token) {
                        // Nothing to restore; the fresh stage is not published.
                        remove_owned_stage(&stage, program, &journal.token)?;
                    }
                } else if stage.exists() {
                    remove_owned_stage(&stage, program, &journal.token)?;
                }
            }
            JournalState::OldMoved => {
                if !root.exists() {
                    if let Some(old) = old.as_ref()
                        && old.exists()
                        && owned_old(
                            old,
                            program,
                            &journal.token,
                            journal.prior_record_sha256.as_deref(),
                        )
                    {
                        fs::rename(old, root).map_err(|error| {
                            format!("the previous install could not be restored: {error}")
                        })?;
                        sync_dir(directory)?;
                    } else {
                        return Err(
                            "the previous install was moved aside and cannot be verified; the journal was kept"
                                .to_owned(),
                        );
                    }
                }
                if stage.exists() {
                    remove_owned_stage(&stage, program, &journal.token)?;
                }
                if let Some(old) = old.as_ref()
                    && old.exists()
                    && owned_old(
                        old,
                        program,
                        &journal.token,
                        journal.prior_record_sha256.as_deref(),
                    )
                {
                    let _ = fs::remove_dir_all(old);
                }
            }
            JournalState::Published => {
                if stage.exists() {
                    remove_owned_stage(&stage, program, &journal.token)?;
                }
                if let Some(old) = old.as_ref()
                    && old.exists()
                    && owned_old(
                        old,
                        program,
                        &journal.token,
                        journal.prior_record_sha256.as_deref(),
                    )
                {
                    if !root.exists() {
                        // The new root was removed after publication; the old version
                        // is the only usable copy and is restored rather than deleted.
                        fs::rename(old, root).map_err(|error| {
                            format!("the previous install could not be restored: {error}")
                        })?;
                        sync_dir(directory)?;
                    } else {
                        let _ = fs::remove_dir_all(old);
                        sync_dir(directory)?;
                    }
                }
            }
        }
        let _ = fs::remove_file(&journal_path);
        sync_dir(directory)?;
    }
    cleanup_owned_stages(directory, program)?;
    cleanup_legacy_stages(directory, root, program)?;
    recover_legacy_old(directory, root, program)?;
    Ok(())
}

/// A crashed pre-journal installer could leave the old root at `<stem>.old-<pid>`
/// with no version root. If exactly one such directory reads back as a complete
/// install of this program, it is the usable old version and is restored.
fn recover_legacy_old(directory: &Path, root: &Path, program: Program) -> Result<(), String> {
    if root.exists() {
        return Ok(());
    }
    let Some(stem) = root.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let stem = Path::new(stem)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(stem);
    let prefix = format!("{stem}.old-");
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = name.strip_prefix(&prefix) else {
            continue;
        };
        if pid.is_empty() || !pid.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if harness::read_installed_at_root(&path, program).is_some() {
            candidates.push(path);
        }
    }
    if candidates.len() != 1 {
        return Ok(());
    }
    fs::rename(&candidates[0], root)
        .map_err(|error| format!("the previous install could not be restored: {error}"))?;
    sync_dir(directory)
}

/// Stages written by the pre-journal installer used `<stem>.part-<pid>`. They have
/// no owner marker, so they are removed only when the exact name pattern and every
/// top-level entry are the package files this installer itself wrote. One unknown
/// entry keeps the directory for a person to inspect.
fn cleanup_legacy_stages(directory: &Path, root: &Path, program: Program) -> Result<(), String> {
    let Some(stem) = root.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let stem = Path::new(stem)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(stem);
    let prefix = format!("{stem}.part-");
    let allowed: &[&str] = match program {
        Program::Pi => &["pi.tar.part", "pi.tar.gz", "pi"],
        Program::Codex => &["codex.tar.part", "codex.tar.gz", "bin"],
        Program::Opencode => &["opencode.part", "opencode.tgz", "package"],
        Program::Dsh => &[
            "package.json",
            "package-lock.json",
            "npm-home",
            "npm-cache",
            "npm-install.log",
            "npm-install.out",
            "node_modules",
        ],
        Program::Builtin => return Ok(()),
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = name.strip_prefix(&prefix) else {
            continue;
        };
        if pid.is_empty() || !pid.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Ok(contents) = fs::read_dir(&path) else {
            continue;
        };
        let ours = contents.flatten().all(|item| {
            let entry_name = item.file_name();
            allowed
                .iter()
                .any(|allowed| entry_name.to_string_lossy() == *allowed)
        });
        if ours {
            let _ = fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

fn cleanup_owned_stages(directory: &Path, program: Program) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(STAGE_PREFIX) {
            continue;
        }
        let path = entry.path();
        if let Some(owner) = read_owner(&path, program)
            && owner.format == FORMAT
            && owner.program == program.id()
            && owner.version == program.pinned_version()
        {
            remove_owned_stage(&path, program, &owner.token)?;
        }
    }
    Ok(())
}

fn remove_owned_stage(stage: &Path, _program: Program, token: &str) -> Result<(), String> {
    if !stage.exists() {
        return Ok(());
    }
    if !stage_owned_by(stage, token) {
        return Err(format!(
            "{} was not created by this install; it was left untouched",
            stage.display()
        ));
    }
    let metadata = fs::symlink_metadata(stage)
        .map_err(|error| format!("{} cannot be checked: {error}", stage.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{} is not a stage directory; it was left untouched",
            stage.display()
        ));
    }
    fs::remove_dir_all(stage)
        .map_err(|error| format!("{} cannot be cleaned up: {error}", stage.display()))
}

fn write_owner(stage: &Path, program: Program, token: &str) -> Result<(), String> {
    let owner = Owner {
        format: FORMAT,
        program: program.id().to_owned(),
        version: program.pinned_version().to_owned(),
        token: token.to_owned(),
    };
    crate::mode::write_private(
        stage.join(OWNER_FILE),
        serde_json::to_string(&owner)
            .map_err(|error| format!("the install owner record cannot be written: {error}"))?
            .as_bytes(),
    )
    .map_err(|error| format!("the install owner record cannot be written: {error}"))
}

fn read_owner(stage: &Path, program: Program) -> Option<Owner> {
    let text = fs::read_to_string(stage.join(OWNER_FILE)).ok()?;
    let owner: Owner = serde_json::from_str(&text).ok()?;
    (owner.program == program.id()).then_some(owner)
}

fn stage_owned_by(stage: &Path, token: &str) -> bool {
    let Ok(metadata) = fs::symlink_metadata(stage) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    let Ok(text) = fs::read_to_string(stage.join(OWNER_FILE)) else {
        return false;
    };
    serde_json::from_str::<Owner>(&text)
        .map(|owner| owner.format == FORMAT && owner.token == token)
        .unwrap_or(false)
}

fn root_owned_by(root: &Path, token: &str) -> bool {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    let Ok(text) = fs::read_to_string(root.join(OWNER_FILE)) else {
        return false;
    };
    serde_json::from_str::<Owner>(&text)
        .map(|owner| owner.format == FORMAT && owner.token == token)
        .unwrap_or(false)
}

fn owned_old(old: &Path, program: Program, token: &str, prior_record_sha256: Option<&str>) -> bool {
    let Ok(metadata) = fs::symlink_metadata(old) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    if stage_owned_by(old, token) {
        return true;
    }
    let Some(expected) = prior_record_sha256 else {
        return false;
    };
    sha256_file(&old.join("install.json")).ok().as_deref() == Some(expected)
        && harness::read_installed_at_root(old, program).is_some()
}

fn validate_owned_name(name: &str, prefix: &str) -> Result<(), String> {
    if !name.starts_with(prefix)
        || name.contains('/')
        || Path::new(name)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("the journal names an unsafe path: {name}"));
    }
    Ok(())
}

fn write_journal(path: &Path, journal: &Journal) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "{} is not the installer's own journal; it was left untouched",
            path.display()
        ));
    }
    let text = serde_json::to_string_pretty(journal)
        .map_err(|error| format!("the install journal cannot be written: {error}"))?;
    crate::mode::write_private(path, text.as_bytes())
        .map_err(|error| format!("the install journal cannot be written: {error}"))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), String> {
    let file = File::open(path)
        .map_err(|error| format!("{} cannot be synced: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("{} cannot be synced: {error}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn json_field(path: &Path, field: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
}

fn file_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| "the install path has no usable name".to_owned())
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut random = [0u8; 8];
    if let Ok(mut file) = File::open("/dev/urandom") {
        let _ = file.read_exact(&mut random);
    }
    format!("{:x}-{:x}-{}", std::process::id(), nanos, hex(&random))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let joined = (first << 16) | (second << 8) | third;
        text.push(TABLE[((joined >> 18) & 63) as usize] as char);
        text.push(TABLE[((joined >> 12) & 63) as usize] as char);
        text.push(if chunk.len() > 1 {
            TABLE[((joined >> 6) & 63) as usize] as char
        } else {
            '='
        });
        text.push(if chunk.len() > 2 {
            TABLE[(joined & 63) as usize] as char
        } else {
            '='
        });
    }
    text
}

#[cfg(debug_assertions)]
fn maybe_pause(point: &str) {
    if std::env::var("GEWU_INSTALL_TEST_PAUSE").ok().as_deref() == Some(point) {
        eprintln!("gewu-multiverse: install test pause at {point}");
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[cfg(not(debug_assertions))]
fn maybe_pause(_point: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dsh_script_policy_is_exactly_the_reviewed_locked_set() {
        verify_dsh_script_policy().expect("the shipped policy matches the shipped lock");
    }

    #[test]
    fn base64_matches_a_known_value() {
        assert_eq!(
            base64(b"Many hands make light work."),
            "TWFueSBoYW5kcyBtYWtlIGxpZ2h0IHdvcmsu"
        );
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
    }

    #[test]
    fn the_declared_dsh_lock_integrity_is_the_vendored_lock_value() {
        let lock: serde_json::Value =
            serde_json::from_slice(include_bytes!("../vendor/dsh/package-lock.json"))
                .expect("the lockfile is JSON");
        assert_eq!(
            lock["packages"]["node_modules/@deepseek-ai/dsh"]["integrity"],
            dsh_lock_integrity()
        );
    }

    #[test]
    fn the_lock_is_exclusive_until_the_first_holder_drops() {
        let root = crate::test_support::scratch_root("installer-lock");
        let first = InstallLock::acquire(&root, Program::Pi).expect("the first lock");
        let second = InstallLock::acquire(&root, Program::Pi);
        assert!(second.is_err(), "a second installer must be refused");
        let error = second.err().unwrap_or_default();
        assert!(error.contains("already being installed"), "{error}");
        drop(first);
        let _third = InstallLock::acquire(&root, Program::Pi).expect("the lock is reusable");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_stage_is_left_alone() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::scratch_root("installer-symlink");
        let foreign = root.join("foreign");
        fs::create_dir_all(&foreign).expect("the foreign directory");
        fs::write(foreign.join("keep"), b"user data").expect("the user file");
        let stage = root.join(".stage-fixture");
        symlink(&foreign, &stage).expect("the symlinked stage");
        let error = remove_owned_stage(&stage, Program::Pi, "token")
            .expect_err("a symlinked stage must not be followed");
        assert!(error.contains("left untouched"), "{error}");
        assert_eq!(
            fs::read(foreign.join("keep")).expect("the user file"),
            b"user data"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_journal_is_left_alone() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::scratch_root("installer-journal-symlink");
        let foreign = root.join("foreign-journal");
        fs::write(&foreign, b"user data").expect("the user file");
        symlink(&foreign, root.join(JOURNAL_FILE)).expect("the symlinked journal");
        let error = recover_locked(&root, &root.join("1.4.0"), Program::Pi)
            .expect_err("a symlinked journal must not be followed");
        assert!(error.contains("left untouched"), "{error}");
        assert_eq!(fs::read(&foreign).expect("the user file"), b"user data");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_record_entrypoint_cannot_escape_its_root() {
        let root = crate::test_support::scratch_root("installer-escape");
        let escaped = InstallRecord {
            program: Program::Pi.id().to_owned(),
            version: Program::Pi.pinned_version().to_owned(),
            sha256: "fixture".to_owned(),
            license: "MIT".to_owned(),
            entrypoint: "../bin/sh".to_owned(),
            resources: None,
            installed_at: "now".to_owned(),
        };
        crate::mode::write_private(
            root.join("install.json"),
            serde_json::to_string(&escaped).expect("json").as_bytes(),
        )
        .expect("the record");
        assert!(harness::read_installed_at_root(&root, Program::Pi).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
