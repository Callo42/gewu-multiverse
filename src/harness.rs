//! The native agent programs the product can run, their tested pins, and the
//! product-owned directories their official packages are installed into.
//!
//! A program is installed by the product, never globally: one versioned directory
//! under the data root, a small `install.json`, and the official package's own
//! files. The built-in engine is not a program here; [`Program::Builtin`] is the
//! legacy path and is represented by an empty saved value.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use crate::installer::{CancelToken, INSTALL_CANCELED, InstallPhase, InstallProgress};

/// A pinned native program. `Builtin` is the product's own direct loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Program {
    Builtin,
    Pi,
    Codex,
    Dsh,
    Opencode,
}

/// The native programs, in display order. Pi is the fresh-install default.
pub const NATIVE_PROGRAMS: [Program; 4] =
    [Program::Pi, Program::Codex, Program::Dsh, Program::Opencode];

/// The official artefact URL for one pinned program.
pub(crate) fn artefact_url(program: Program) -> Option<&'static str> {
    match program {
        Program::Pi => Some(
            "https://github.com/earendil-works/pi/releases/download/v1.4.0/pi-linux-x64.tar.gz",
        ),
        Program::Codex => Some(
            "https://github.com/openai/codex/releases/download/rust-v0.164.0/codex-package-x86_64-unknown-linux-musl.tar.gz",
        ),
        Program::Opencode => {
            Some("https://registry.npmjs.org/@opencode/cli-linux-x64/-/cli-linux-x64-2.1.4.tgz")
        }
        Program::Dsh => Some("https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.2.1-rc.2.tgz"),
        Program::Builtin => None,
    }
}

/// The official artefact SHA-256 for the single-file tarballs.
pub(crate) fn artefact_sha256(program: Program) -> &'static str {
    match program {
        Program::Pi => "f2e4e4329603223d03260cdb8bd5033dbdc1f73ba58ab8939db2e56546e115d2",
        Program::Codex => "3ec55053109608d5038f7e50750f7d25f768dfd740fef071aa710e2559b63aac",
        Program::Opencode => "ce05abe2dce1041391c8ec8cdb8da066f640fd471e72ac846dcb089c441e11e1",
        Program::Dsh => "40e3d025c0240c4316ff42dee9bedc7709c0fb38aea10543b47076ed2af52e1d",
        Program::Builtin => "",
    }
}

/// One official package address and its declared SHA-256. Debug builds accept a
/// test-only override so the installer's own paths can be exercised without the
/// public network; release builds never read those variables.
pub(crate) struct Artifact {
    pub url: String,
    pub sha256: String,
}

#[cfg(test)]
static TEST_ARTIFACT: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// Unit tests in this crate use a loopback server without touching process
/// environment variables. Integration tests exercise the debug-only environment
/// override in a child process instead.
#[cfg(test)]
pub(crate) fn set_test_artifact(url: Option<&str>, sha256: Option<&str>) {
    if let Ok(mut slot) = TEST_ARTIFACT.lock() {
        *slot = url
            .zip(sha256)
            .map(|(url, sha)| (url.to_owned(), sha.to_owned()));
    }
}

pub(crate) fn artifact(program: Program) -> Option<Artifact> {
    #[cfg(test)]
    if let Some((url, sha256)) = TEST_ARTIFACT.lock().ok().and_then(|slot| slot.clone()) {
        return Some(Artifact { url, sha256 });
    }
    #[cfg(debug_assertions)]
    if let (Ok(url), Ok(sha256)) = (
        std::env::var("GEWU_INSTALL_TEST_URL"),
        std::env::var("GEWU_INSTALL_TEST_SHA256"),
    ) && !url.is_empty()
        && !sha256.is_empty()
    {
        return Some(Artifact { url, sha256 });
    }
    Some(Artifact {
        url: artefact_url(program)?.to_owned(),
        sha256: artefact_sha256(program).to_owned(),
    })
}

/// The wire/provider protocol a connection speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl Protocol {
    /// The saved/CLI word.
    pub fn id(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    /// Parse a saved/CLI word.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "chat-completions" | "chat" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "anthropic-messages" | "anthropic" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    /// The user-facing name, not the wire word.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "Chat Completions",
            Self::Responses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
        }
    }
}

impl Program {
    /// The saved id; empty is the legacy built-in value.
    pub fn id(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Pi => "pi",
            Self::Codex => "codex",
            Self::Dsh => "dsh",
            Self::Opencode => "opencode",
        }
    }

    /// Parse one saved/CLI word. An empty value is the built-in engine.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "" | "builtin" | "built-in" => Some(Self::Builtin),
            "pi" => Some(Self::Pi),
            "codex" => Some(Self::Codex),
            "dsh" => Some(Self::Dsh),
            "opencode" | "open-code" => Some(Self::Opencode),
            _ => None,
        }
    }

    /// The product's display name. It is a proper program name, not a developer term.
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "built-in engine",
            Self::Pi => "Pi",
            Self::Codex => "Codex",
            Self::Dsh => "DeepSeek Harness",
            Self::Opencode => "OpenCode",
        }
    }

    /// The tested version this product installs and describes.
    pub fn pinned_version(self) -> &'static str {
        match self {
            Self::Builtin => "",
            Self::Pi => "1.4.0",
            Self::Codex => "0.164.0",
            Self::Dsh => "0.2.1-rc.2",
            Self::Opencode => "2.1.4",
        }
    }

    /// The licence of the official package.
    pub fn license(self) -> &'static str {
        match self {
            Self::Builtin => "MIT OR Apache-2.0",
            Self::Pi | Self::Dsh | Self::Opencode => "MIT",
            Self::Codex => "Apache-2.0",
        }
    }

    /// The approximate download size a person sees before a fetch.
    pub fn download_mb(self) -> u32 {
        match self {
            Self::Builtin => 0,
            Self::Pi => 43,
            Self::Codex => 162,
            Self::Dsh => 753,
            Self::Opencode => 240,
        }
    }

    /// What the program needs on the machine besides its own files.
    pub fn prerequisite(self) -> Option<&'static str> {
        match self {
            Self::Builtin | Self::Pi | Self::Codex | Self::Opencode => None,
            Self::Dsh => Some("Node.js 24.9+ and npm"),
        }
    }

    /// The protocols the native adapter can configure for this program.
    pub fn supported_protocols(self) -> &'static [Protocol] {
        match self {
            Self::Builtin => &[Protocol::ChatCompletions],
            Self::Codex => &[Protocol::Responses],
            Self::Pi | Self::Dsh | Self::Opencode => &[
                Protocol::ChatCompletions,
                Protocol::Responses,
                Protocol::AnthropicMessages,
            ],
        }
    }

    /// Whether this protocol can be selected for the program.
    pub fn supports(self, protocol: Protocol) -> bool {
        self.supported_protocols().contains(&protocol)
    }

    /// Whether the program is a native CLI rather than the built-in loop.
    pub fn is_native(self) -> bool {
        !matches!(self, Self::Builtin)
    }

    /// The environment label for a missing prerequisite.
    pub fn prerequisite_missing(self) -> Option<&'static str> {
        if self != Self::Dsh {
            return None;
        }
        if !command_exists("node") || !command_exists("npm") {
            return Some("Node.js 24.9 or newer and npm are needed for DeepSeek Harness");
        }
        let output = match std::process::Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => output,
            _ => return Some("Node.js 24.9 or newer is needed for DeepSeek Harness"),
        };
        let version = String::from_utf8_lossy(&output.stdout);
        let version = version.trim().trim_start_matches('v');
        let mut pieces = version.split('.');
        let major = pieces.next().and_then(|piece| piece.parse::<u32>().ok());
        let minor = pieces.next().and_then(|piece| piece.parse::<u32>().ok());
        let Some((major, minor)) = major.zip(minor) else {
            return Some("Node.js 24.9 or newer is needed for DeepSeek Harness");
        };
        if major < 24 || (major == 24 && minor < 9) {
            return Some("Node.js 24.9 or newer is needed for DeepSeek Harness");
        }
        if !std::process::Command::new("npm")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return Some("npm is needed for DeepSeek Harness");
        }
        None
    }
}

/// Whether a command is on `PATH` and executable.
pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
                    && fs::metadata(&candidate)
                        .map(|metadata| {
                            use std::os::unix::fs::PermissionsExt;
                            metadata.permissions().mode() & 0o111 != 0
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// The install metadata stored beside a program's files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRecord {
    /// The program id, so a moved/renamed directory is not mistaken for another.
    pub program: String,
    /// The exact version of the official package.
    pub version: String,
    /// The artifact SHA-256 verified at install.
    pub sha256: String,
    /// The licence of the artifact.
    pub license: String,
    /// The executable, relative to the install root.
    pub entrypoint: String,
    /// The directory holding official package resources, relative to the root.
    #[serde(default)]
    pub resources: Option<String>,
    /// When it was installed, RFC 3339.
    pub installed_at: String,
}

/// One installed native program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledProgram {
    /// The pinned program.
    pub program: Program,
    /// The version recorded by the installer.
    pub version: String,
    /// The versioned install directory.
    pub root: PathBuf,
    /// The executable the product launches.
    pub executable: PathBuf,
    /// The artefact hash recorded at install.
    pub sha256: String,
    /// The licence recorded at install.
    pub license: String,
}

/// The product-owned directory all native programs live under.
pub fn harnesses_root(data_root: &Path) -> PathBuf {
    data_root.join("harnesses")
}

/// The versioned directory for one program.
pub fn install_root(data_root: &Path, program: Program) -> PathBuf {
    harnesses_root(data_root)
        .join(program.id())
        .join(program.pinned_version())
}

/// The metadata file inside one install root.
pub fn record_path(root: &Path) -> PathBuf {
    root.join("install.json")
}

/// Read one installed program, if its record and executable are present.
///
/// The pinned version is the only version this product installs. A record with a
/// different version is still read (and shown as not tested) rather than silently
/// treated as the pin; the caller sees the actual version.
pub fn installed(data_root: &Path, program: Program) -> Option<InstalledProgram> {
    read_installed_at_root(&install_root(data_root, program), program)
}

/// Read one install at an explicit root (the published root, or a completed stage
/// before it is renamed). A record is not enough: the entrypoint must be a real
/// executable below the root, and DeepSeek Harness must still carry its native
/// terminal and FFI helpers.
pub(crate) fn read_installed_at_root(root: &Path, program: Program) -> Option<InstalledProgram> {
    if !program.is_native() {
        return None;
    }
    let root_metadata = fs::symlink_metadata(root).ok()?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return None;
    }
    let text = fs::read_to_string(record_path(root)).ok()?;
    let record: InstallRecord = serde_json::from_str(&text).ok()?;
    if record.program != program.id() {
        return None;
    }
    let relative = safe_relative(&record.entrypoint)?;
    let executable = root.join(relative);
    let root_canon = fs::canonicalize(root).ok()?;
    let executable_canon = fs::canonicalize(&executable).ok()?;
    if !executable_canon.starts_with(&root_canon) {
        return None;
    }
    let metadata = fs::metadata(&executable).ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return None;
    }
    if program == Program::Dsh && !dsh_helpers_ready(root) {
        return None;
    }
    Some(InstalledProgram {
        program,
        version: record.version,
        root: root.to_path_buf(),
        executable,
        sha256: record.sha256,
        license: record.license,
    })
}

fn safe_relative(text: &str) -> Option<PathBuf> {
    let path = Path::new(text);
    if text.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn node_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

fn dsh_helpers_ready(root: &Path) -> bool {
    let modules = root.join("node_modules");
    let package = modules.join("@deepseek-ai/dsh/package.json");
    let Some(value) = fs::read_to_string(package)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return false;
    };
    if value.get("version").and_then(serde_json::Value::as_str) != Some("0.2.1-rc.2") {
        return false;
    }
    let platform = node_platform();
    if ![
        modules.join(format!("node-pty/prebuilds/{platform}/pty.node")),
        modules.join("node-pty/build/Release/pty.node"),
        modules.join("node-pty/build/Debug/pty.node"),
    ]
    .into_iter()
    .any(|path| path.is_file())
    {
        return false;
    }
    if !modules.join("koffi/package.json").is_file() || !koffi_native_present(&modules) {
        return false;
    }
    for helper in [
        modules.join(format!("node-pty/prebuilds/{platform}/spawn-helper")),
        modules.join("node-pty/build/Release/spawn-helper"),
    ] {
        if helper.is_file()
            && fs::metadata(&helper)
                .map(|metadata| metadata.permissions().mode() & 0o111 == 0)
                .unwrap_or(true)
        {
            return false;
        }
    }
    true
}

fn koffi_native_present(modules: &Path) -> bool {
    fn has_node_file(path: &Path, depth: u8) -> bool {
        let Ok(entries) = fs::read_dir(path) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "node")
            {
                return true;
            }
            if depth > 0 && path.is_dir() && has_node_file(&path, depth - 1) {
                return true;
            }
        }
        false
    }
    if has_node_file(&modules.join("@koromix"), 2) {
        return true;
    }
    // A source build puts the binding below koffi/build; that is still the helper
    // the locked package asked its install script to produce.
    modules.join("koffi/build").is_dir() && has_node_file(&modules.join("koffi/build"), 3)
}

/// The installed state of every native program, in display order.
pub fn installed_all(data_root: &Path) -> Vec<(Program, Option<InstalledProgram>)> {
    NATIVE_PROGRAMS
        .into_iter()
        .map(|program| (program, installed(data_root, program)))
        .collect()
}

/// Install one pinned program into the product's data directory. The previous
/// usable version is kept until the new stage is complete and verified.
pub fn install(
    data_root: &Path,
    program: Program,
    progress: impl FnMut(Option<u8>),
) -> Result<InstalledProgram, String> {
    let cancel = CancelToken::new();
    let mut progress = progress;
    install_cancellable(data_root, program, &cancel, |sample| {
        progress(sample.percent)
    })
}

/// The cancellable install the board and the installer flag use. The callback is
/// called for every phase, so an unknown percentage is never shown as a number.
pub fn install_cancellable(
    data_root: &Path,
    program: Program,
    cancel: &CancelToken,
    progress: impl FnMut(InstallProgress),
) -> Result<InstalledProgram, String> {
    crate::installer::install(data_root, program, cancel, progress)
}

/// Repair interrupted installs before the first picture is drawn.
pub fn recover_pending(data_root: &Path) -> Vec<String> {
    crate::installer::recover_pending(data_root)
}

/// Why a selected program/protocol cannot start, in one product sentence.
pub fn compatibility_error(program: Program, protocol: Protocol) -> Option<String> {
    match program {
        Program::Builtin => (protocol != Protocol::ChatCompletions)
            .then(|| "the built-in engine needs a Chat Completions connection".to_owned()),
        Program::Codex => (protocol != Protocol::Responses).then(|| {
            "Codex needs an OpenAI Responses connection; choose Responses or another program"
                .to_owned()
        }),
        Program::Pi | Program::Dsh | Program::Opencode => {
            (!program.supports(protocol)).then(|| {
                format!(
                    "{} cannot use a {} connection",
                    program.label(),
                    protocol.label()
                )
            })
        }
    }
}

/// Current UTC time in RFC 3339, for the install record.
pub fn now() -> String {
    crate::time::rfc3339(crate::time::now_unix_ms())
}

/// Whether a path is inside another path, after resolving what exists.
pub fn path_inside(path: &Path, root: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_native_programs_are_pinned_and_licensed() {
        for program in NATIVE_PROGRAMS {
            assert!(!program.pinned_version().is_empty());
            assert!(!program.license().is_empty());
            assert!(!program.label().is_empty());
            assert!(program.is_native());
        }
        assert_eq!(Program::parse(""), Some(Program::Builtin));
        assert_eq!(Program::parse("pi"), Some(Program::Pi));
        assert_eq!(Program::parse("potato"), None);
    }

    #[test]
    fn codex_is_responses_only_and_the_others_are_broader() {
        assert_eq!(Program::Codex.supported_protocols(), &[Protocol::Responses]);
        assert!(compatibility_error(Program::Codex, Protocol::ChatCompletions).is_some());
        assert!(compatibility_error(Program::Codex, Protocol::Responses).is_none());
        assert!(compatibility_error(Program::Pi, Protocol::AnthropicMessages).is_none());
    }

    #[test]
    fn the_protocol_matrix_is_explicit_and_codex_is_responses_only() {
        assert_eq!(Program::Codex.supported_protocols(), &[Protocol::Responses]);
        assert!(Program::Pi.supports(Protocol::ChatCompletions));
        assert!(Program::Pi.supports(Protocol::AnthropicMessages));
        assert!(Program::Dsh.supports(Protocol::Responses));
        assert!(Program::Opencode.supports(Protocol::AnthropicMessages));
        assert!(Program::Builtin.supports(Protocol::ChatCompletions));
        assert!(compatibility_error(Program::Codex, Protocol::ChatCompletions).is_some());
    }

    #[test]
    fn a_record_only_names_an_installed_program_when_its_entrypoint_exists() {
        let root = std::env::temp_dir().join(format!(
            "gewu-harness-record-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let install = install_root(&root, Program::Pi);
        std::fs::create_dir_all(install.join("bin")).expect("the fixture root");
        let executable = install.join("bin/pi");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("the fixture binary");
        let mut mode = std::fs::metadata(&executable).expect("mode").permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&executable, mode).expect("the fixture mode");
        let record = InstallRecord {
            program: Program::Pi.id().to_owned(),
            version: Program::Pi.pinned_version().to_owned(),
            sha256: "fixture".to_owned(),
            license: "MIT".to_owned(),
            entrypoint: "bin/pi".to_owned(),
            resources: None,
            installed_at: "now".to_owned(),
        };
        crate::mode::write_private(
            record_path(&install),
            serde_json::to_string(&record).expect("json").as_bytes(),
        )
        .expect("the record");
        assert_eq!(
            installed(&root, Program::Pi).expect("installed").executable,
            executable
        );
        std::fs::remove_file(&executable).expect("remove the binary");
        assert!(installed(&root, Program::Pi).is_none());
        assert!(installed(&root, Program::Codex).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_dsh_lockfile_is_the_locked_official_closure() {
        let lock: serde_json::Value =
            serde_json::from_slice(include_bytes!("../vendor/dsh/package-lock.json"))
                .expect("the lockfile is JSON");
        assert_eq!(lock["lockfileVersion"], 3);
        assert_eq!(
            lock["packages"][""]["dependencies"]["@deepseek-ai/dsh"],
            "^0.2.1-rc.2"
        );
        assert_eq!(
            lock["packages"]["node_modules/@deepseek-ai/dsh"]["version"],
            "0.2.1-rc.2"
        );
        assert!(
            lock["packages"]
                .as_object()
                .is_some_and(|packages| packages.len() > 500),
            "the lock must carry the transitive closure"
        );
    }
}
