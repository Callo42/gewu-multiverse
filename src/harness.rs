//! The native agent programs the product can run, their tested pins, and the
//! product-owned directories their official packages are installed into.
//!
//! A program is installed by the product, never globally: one versioned directory
//! under the data root, a small `install.json`, and the official package's own
//! files. The built-in engine is not a program here; [`Program::Builtin`] is the
//! legacy path and is represented by an empty saved value.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use serde::{Deserialize, Serialize};

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
fn artefact_url(program: Program) -> Option<&'static str> {
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
        Program::Dsh => None,
        Program::Builtin => None,
    }
}

/// The official artefact SHA-256 for the single-file tarballs.
fn artefact_sha256(program: Program) -> &'static str {
    match program {
        Program::Pi => "f2e4e4329603223d03260cdb8bd5033dbdc1f73ba58ab8939db2e56546e115d2",
        Program::Codex => "3ec55053109608d5038f7e50750f7d25f768dfd740fef071aa710e2559b63aac",
        Program::Opencode => "ce05abe2dce1041391c8ec8cdb8da066f640fd471e72ac846dcb089c441e11e1",
        Program::Dsh => "40e3d025c0240c4316ff42dee9bedc7709c0fb38aea10543b47076ed2af52e1d",
        Program::Builtin => "",
    }
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
            Self::Dsh => Some("Node.js 24.9 or newer and npm"),
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
        let output = std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout);
        let version = version.trim().trim_start_matches('v');
        let mut pieces = version.split('.');
        let major: u32 = pieces.next()?.parse().ok()?;
        let minor: u32 = pieces.next()?.parse().ok()?;
        if major < 24 || (major == 24 && minor < 9) {
            return Some("Node.js 24.9 or newer is needed for DeepSeek Harness");
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
    if !program.is_native() {
        return None;
    }
    let root = install_root(data_root, program);
    let text = fs::read_to_string(record_path(&root)).ok()?;
    let record: InstallRecord = serde_json::from_str(&text).ok()?;
    if record.program != program.id() {
        return None;
    }
    let executable = root.join(&record.entrypoint);
    if !executable.is_file() {
        return None;
    }
    Some(InstalledProgram {
        program,
        version: record.version,
        root,
        executable,
        sha256: record.sha256,
        license: record.license,
    })
}

/// The installed state of every native program, in display order.
pub fn installed_all(data_root: &Path) -> Vec<(Program, Option<InstalledProgram>)> {
    NATIVE_PROGRAMS
        .into_iter()
        .map(|program| (program, installed(data_root, program)))
        .collect()
}

/// Install one pinned program into the product's data directory. Old installs are
/// preserved until the new stage is complete; a failure removes only the stage.
pub fn install(
    data_root: &Path,
    program: Program,
    mut progress: impl FnMut(Option<u8>),
) -> Result<InstalledProgram, String> {
    if !program.is_native() {
        return Err("the built-in engine needs no install".to_owned());
    }
    if let Some(existing) = installed(data_root, program)
        && existing.version == program.pinned_version()
    {
        return Ok(existing);
    }
    if let Some(prerequisite) = program.prerequisite_missing() {
        return Err(prerequisite.to_owned());
    }
    let root = install_root(data_root, program);
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("{} cannot be created: {error}", parent.display()))?;
    }
    let stage = root.with_extension(format!("part-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)
        .map_err(|error| format!("{} cannot be created: {error}", stage.display()))?;
    let result = (|| -> Result<(), String> {
        match program {
            Program::Pi => {
                let archive = stage.join("pi.tar.gz");
                download(
                    artefact_url(program).unwrap(),
                    &archive,
                    artefact_sha256(program),
                    &mut progress,
                )?;
                extract(&archive, &stage)?;
                if !stage.join("pi/pi").is_file() {
                    return Err("the Pi package did not contain the expected program".to_owned());
                }
            }
            Program::Codex => {
                let archive = stage.join("codex.tar.gz");
                download(
                    artefact_url(program).unwrap(),
                    &archive,
                    artefact_sha256(program),
                    &mut progress,
                )?;
                extract(&archive, &stage)?;
                if !stage.join("bin/codex").is_file() {
                    return Err("the Codex package did not contain the expected program".to_owned());
                }
            }
            Program::Opencode => {
                let archive = stage.join("opencode.tgz");
                download(
                    artefact_url(program).unwrap(),
                    &archive,
                    artefact_sha256(program),
                    &mut progress,
                )?;
                extract(&archive, &stage)?;
                if !stage.join("package/bin/opencode").is_file() {
                    return Err(
                        "the OpenCode package did not contain the expected program".to_owned()
                    );
                }
            }
            Program::Dsh => install_dsh(&stage, &mut progress)?,
            Program::Builtin => unreachable!(),
        }
        let entrypoint = match program {
            Program::Pi => "pi/pi",
            Program::Codex => "bin/codex",
            Program::Opencode => "package/bin/opencode",
            Program::Dsh => "node_modules/.bin/dsh",
            Program::Builtin => unreachable!(),
        };
        let resources = (program == Program::Dsh).then(|| {
            format!(
                "npm lock sha256 {}",
                hex(&Sha256::digest(include_bytes!(
                    "../vendor/dsh/package-lock.json"
                )))
            )
        });
        let record = InstallRecord {
            program: program.id().to_owned(),
            version: program.pinned_version().to_owned(),
            sha256: artefact_sha256(program).to_owned(),
            license: program.license().to_owned(),
            entrypoint: entrypoint.to_owned(),
            resources,
            installed_at: now(),
        };
        let text = serde_json::to_string_pretty(&record)
            .map_err(|error| format!("the install record cannot be written: {error}"))?;
        crate::mode::write_private(record_path(&stage), text.as_bytes())
            .map_err(|error| format!("the install record cannot be written: {error}"))?;
        let _ = crate::mode::set_mode(&stage, crate::mode::PRIVATE_DIR_MODE);
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let old = root.with_extension(format!("old-{}", std::process::id()));
    let had_old = root.exists();
    if had_old {
        fs::rename(&root, &old).map_err(|error| {
            format!(
                "the old {} install cannot be moved aside: {error}",
                program.label()
            )
        })?;
    }
    if let Err(error) = fs::rename(&stage, &root) {
        if had_old {
            let _ = fs::rename(&old, &root);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!(
            "{} cannot be put in place: {error}",
            program.label()
        ));
    }
    let _ = fs::remove_dir_all(&old);
    installed(data_root, program).ok_or_else(|| {
        format!(
            "{} was installed but its record cannot be read back",
            program.label()
        )
    })
}

fn download(
    url: &str,
    target: &Path,
    expected_sha256: &str,
    progress: &mut impl FnMut(Option<u8>),
) -> Result<(), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|error| format!("{url} could not be reached ({error})"))?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        return Err(format!("{url} answered HTTP {status}"));
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
        .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
    let mut reader = response.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("{url} stopped while downloading: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
        received += read as u64;
        progress(
            total
                .filter(|total| *total > 0)
                .map(|total| ((received.saturating_mul(100) / total).min(100)) as u8),
        );
    }
    file.sync_all()
        .map_err(|error| format!("{} cannot be synced: {error}", stage.display()))?;
    drop(file);
    let actual = hex(&hasher.finalize());
    if actual != expected_sha256 {
        let _ = fs::remove_file(&stage);
        return Err(format!(
            "{url} did not match its published checksum (got {actual})"
        ));
    }
    fs::rename(&stage, target)
        .map_err(|error| format!("{} cannot be put in place: {error}", target.display()))
}

fn extract(archive: &Path, target: &Path) -> Result<(), String> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(target)
        .status()
        .map_err(|error| format!("the archive cannot be unpacked: {error}"))?;
    if !status.success() {
        return Err("the archive could not be unpacked".to_owned());
    }
    let _ = fs::remove_file(archive);
    Ok(())
}

fn install_dsh(stage: &Path, progress: &mut impl FnMut(Option<u8>)) -> Result<(), String> {
    progress(None);
    let home = stage.join("npm-home");
    let cache = stage.join("npm-cache");
    fs::create_dir_all(&home).map_err(|error| format!("{error}"))?;
    fs::create_dir_all(&cache).map_err(|error| format!("{error}"))?;
    crate::mode::write_private(
        stage.join("package.json"),
        include_bytes!("../vendor/dsh/package.json"),
    )
    .map_err(|error| format!("the DeepSeek Harness package file cannot be written: {error}"))?;
    crate::mode::write_private(
        stage.join("package-lock.json"),
        include_bytes!("../vendor/dsh/package-lock.json"),
    )
    .map_err(|error| format!("the DeepSeek Harness lockfile cannot be written: {error}"))?;
    let status = std::process::Command::new("npm")
        .args([
            "ci",
            "--no-audit",
            "--no-fund",
            "--prefix",
            stage.to_str().unwrap_or("."),
        ])
        .env("HOME", &home)
        .env("npm_config_cache", &cache)
        .status()
        .map_err(|error| format!("npm could not be started: {error}"))?;
    if !status.success() {
        return Err("npm could not install DeepSeek Harness from its locked packages".to_owned());
    }
    let package = stage.join("node_modules/@deepseek-ai/dsh/package.json");
    let text = fs::read_to_string(&package).map_err(|error| {
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
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&cache);
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
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
    fn a_wrong_checksum_leaves_no_part_and_the_right_one_installs() {
        let root = std::env::temp_dir().join(format!(
            "gewu-harness-download-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("the fixture root");
        let body = b"an official package stand-in";
        let source = crate::test_stub::Stub::start(vec![crate::test_stub::StubReply::status(
            200,
            String::from_utf8_lossy(body).into_owned(),
        )]);
        let target = root.join("package");
        let error = download(&source.url(), &target, &"0".repeat(64), &mut |_| {})
            .expect_err("a wrong checksum is refused");
        assert!(error.contains("checksum"), "{error}");
        assert!(!target.exists());
        assert!(!target.with_extension("part").exists());
        let digest = Sha256::digest(body);
        let digest = hex(&digest);
        download(&source.url(), &target, &digest, &mut |_| {}).expect("the right checksum");
        assert_eq!(std::fs::read(&target).expect("the package"), body);
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
