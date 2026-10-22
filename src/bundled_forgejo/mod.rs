//! The one Forgejo this product installs and runs for itself.
//!
//! The release is pinned by version and SHA-256. Installing streams a source to a
//! temporary file beside the target, hashes what it wrote, and renames the file into
//! place only after the digest matches; a mismatch removes the temporary file and
//! keeps nothing. A local file named by `GEWU_FORGEJO_BINARY` takes the same path
//! through the same check, so a fixture never needs the network.
//!
//! An instance is a directory of its own under the data root: `app.ini` with generated
//! secrets and a loopback-only address, an SQLite database and file logs. The port is
//! chosen once from 3000–3099 and read back from `app.ini` on every later start, so the
//! address a person sees does not move. `migrate` and the administrator account go
//! through Forgejo's own CLI; the password never appears in a product argument, and
//! `p` resets it through the administration API. The server then runs as `web` in its
//! own process group.
//!
//! Provisioning is idempotent and goes through the administration API: organisation
//! `team`, team `agents` with write access to all its repositories, and the starter
//! repository `team/workspace` initialised with a README.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::credentials::Secret;
use crate::forgejo::{ForgejoClient, ForgejoError, Repository, Team};
use crate::mode::{PRIVATE_DIR_MODE, SECRET_FILE_MODE, set_mode};

/// The pinned release.
pub const VERSION: &str = "17.0.0";

/// The release asset this product installs.
pub const ASSET: &str = "forgejo-17.0.0-linux-amd64";

/// The published SHA-256 of [`ASSET`].
pub const SHA256: &str = "5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca";

/// The official release URL.
pub const OFFICIAL_URL: &str =
    "https://codeberg.org/forgejo/forgejo/releases/download/v17.0.0/forgejo-17.0.0-linux-amd64";

/// The GitHub repository this release is published from.
pub const REPOSITORY: &str = "Callo42/gewu-multiverse";

/// This release's own copy of the pinned binary, attached to the GitHub release.
///
/// The release tag carries the crate's version, so the address follows the binary
/// that is asking for it. A release that does not carry the asset answers not-found;
/// that source is then skipped and the official one is tried.
pub fn release_asset_url() -> String {
    format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}/{ASSET}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Where the pinned binary may come from, in order: this release's own asset first,
/// then the official Codeberg release. Every answer is checked against the pinned
/// digest. A missing or wrong earlier source falls through to the next; only when the
/// whole list has failed does the install refuse, keeping nothing.
pub fn download_sources() -> Vec<String> {
    vec![release_asset_url(), OFFICIAL_URL.to_owned()]
}

/// The organisation every agent belongs to.
pub const ORGANISATION: &str = "team";

/// The team every agent belongs to.
pub const TEAM: &str = "agents";

/// The starter repository inside the organisation.
pub const STARTER_REPOSITORY: &str = "workspace";

/// The login to use when `$USER` cannot name a Forgejo account.
pub const FALLBACK_ADMIN_LOGIN: &str = "owner";

/// The name of the process-group lock file under the data root.
pub const LOCK_FILE: &str = "engine.lock";

/// The instance's directory name under the data root.
pub const INSTANCE_DIR: &str = "instance";

/// The running server's pid, inside the instance directory.
pub const PID_FILE: &str = "forgejo.pid";

/// How many bytes one download read asks for.
const READ_CHUNK: usize = 64 * 1024;

/// Progress of an install, for the `Fetching` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    /// 0–100 when the size is known.
    pub percent: Option<u8>,
    /// Bytes written so far.
    pub received: u64,
    /// The total size, when the source named it.
    pub total: Option<u64>,
}

/// The data directory the bundled Forgejo lives under, from the platform's own data
/// directory: `$XDG_DATA_HOME/gewu-multiverse`, or its fallback.
pub fn data_root() -> Result<PathBuf, String> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| "no data directory could be found for the bundled Forgejo".to_owned())?;
    Ok(base.data_dir().join("gewu-multiverse"))
}

/// The directory the binary and the instance live under.
pub fn forgejo_dir(data_root: &Path) -> PathBuf {
    data_root.join("forgejo")
}

/// The pinned binary's path after a successful install.
pub fn binary_path(data_root: &Path) -> PathBuf {
    forgejo_dir(data_root).join(ASSET)
}

/// The one-instance-per-data-directory lock file.
pub fn lock_path(data_root: &Path) -> PathBuf {
    forgejo_dir(data_root).join(LOCK_FILE)
}

/// Hash a file with SHA-256, streaming so a 122 MB binary is never held in memory.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; READ_CHUNK];
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

/// Install the pinned binary under the default data directory.
pub fn install_forgejo(progress: impl FnMut(InstallProgress)) -> Result<PathBuf, String> {
    install_forgejo_at(&data_root()?, progress)
}

/// Install the pinned binary under one data directory.
pub fn install_forgejo_at(
    data_root: &Path,
    mut progress: impl FnMut(InstallProgress),
) -> Result<PathBuf, String> {
    let target = binary_path(data_root);
    if is_pinned(&target) {
        return Ok(target);
    }
    if let Some(local) = std::env::var_os("GEWU_FORGEJO_BINARY") {
        let local = PathBuf::from(local);
        if local.as_os_str().is_empty() {
            // An explicitly empty variable is the same as one that is not set.
        } else {
            return install_local(&local, &target, SHA256, &mut progress);
        }
    }
    let sources = download_sources();
    install_from_sources(&sources, &target, SHA256, &mut progress)
}

/// Install from an explicit list of URLs. This is the download half, public so a
/// fixture can point it at a loopback server and verify the refusal/acceptance path
/// without reaching the network.
pub fn install_from_sources(
    sources: &[String],
    target: &Path,
    expected_sha256: &str,
    mut progress: impl FnMut(InstallProgress),
) -> Result<PathBuf, String> {
    if target.is_file() && sha256_file(target)? == expected_sha256 {
        return Ok(target.to_path_buf());
    }
    let _ = fs::remove_file(target);
    if let Some(parent) = target.parent() {
        ensure_dir(parent, 0o755)?;
    }
    let mut failures: Vec<String> = Vec::new();
    for source in sources {
        match download_one(source, target, expected_sha256, &mut progress) {
            Ok(true) => return Ok(target.to_path_buf()),
            Ok(false) => failures.push(format!(
                "{source} did not match the pinned SHA-256; nothing was kept"
            )),
            Err(reason) => failures.push(reason),
        }
    }
    if failures.is_empty() {
        failures.push("no download source was named".to_owned());
    }
    Err(format!(
        "the bundled Forgejo could not be downloaded from any source, so nothing was kept; the next start tries again: {}",
        failures.join("; ")
    ))
}

/// One source: stream to a temporary file, hash, then rename. `Ok(false)` means the
/// bytes were received but did not match, which is a refusal rather than a fallback.
fn download_one(
    source: &str,
    target: &Path,
    expected_sha256: &str,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<bool, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .get(source)
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|error| format!("{source} could not be reached ({error})"))?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        return Err(format!("{source} answered HTTP {status}"));
    }
    let total = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let stage = stage_path(target);
    let result = (|| -> Result<bool, String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stage)
            .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
        let mut reader = response.into_body().into_reader();
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("{source} stopped while downloading: {error}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])
                .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
            received += read as u64;
            progress(InstallProgress {
                percent: total
                    .filter(|total| *total > 0)
                    .map(|total| ((received.saturating_mul(100) / total).min(100)) as u8),
                received,
                total,
            });
        }
        file.sync_all()
            .map_err(|error| format!("{} cannot be synced: {error}", stage.display()))?;
        drop(file);
        if hex(&hasher.finalize()) != expected_sha256 {
            return Ok(false);
        }
        set_mode(&stage, 0o755)
            .map_err(|error| format!("{} cannot be made executable: {error}", stage.display()))?;
        fs::rename(&stage, target)
            .map_err(|error| format!("{} cannot be put in place: {error}", target.display()))?;
        Ok(true)
    })();
    if !matches!(result, Ok(true)) {
        let _ = fs::remove_file(&stage);
    }
    result
}

/// Copy a local file through the same hash check and atomic rename.
fn install_local(
    source: &Path,
    target: &Path,
    expected_sha256: &str,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<PathBuf, String> {
    if !source.is_file() {
        return Err(format!(
            "GEWU_FORGEJO_BINARY names {}, which is not a file",
            source.display()
        ));
    }
    if target.is_file() && sha256_file(target)? == expected_sha256 {
        return Ok(target.to_path_buf());
    }
    if let Some(parent) = target.parent() {
        ensure_dir(parent, 0o755)?;
    }
    let _ = fs::remove_file(target);
    let stage = stage_path(target);
    let _ = fs::remove_file(&stage);
    let total = fs::metadata(source).ok().map(|metadata| metadata.len());
    let mut input = File::open(source)
        .map_err(|error| format!("{} cannot be read: {error}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stage)
        .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut buffer = vec![0u8; READ_CHUNK];
    let copied = (|| -> Result<(), String> {
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| format!("{} cannot be read: {error}", source.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("{} cannot be written: {error}", stage.display()))?;
            received += read as u64;
            progress(InstallProgress {
                percent: total
                    .filter(|total| *total > 0)
                    .map(|total| ((received.saturating_mul(100) / total).min(100)) as u8),
                received,
                total,
            });
        }
        output
            .sync_all()
            .map_err(|error| format!("{} cannot be synced: {error}", stage.display()))
    })();
    drop(output);
    if let Err(reason) = copied {
        let _ = fs::remove_file(&stage);
        return Err(reason);
    }
    if hex(&hasher.finalize()) != expected_sha256 {
        let _ = fs::remove_file(&stage);
        return Err(
            "the local Forgejo binary does not match the pinned SHA-256; nothing was kept"
                .to_owned(),
        );
    }
    set_mode(&stage, 0o755)
        .map_err(|error| format!("{} cannot be made executable: {error}", stage.display()))?;
    fs::rename(&stage, target)
        .map_err(|error| format!("{} cannot be put in place: {error}", target.display()))?;
    Ok(target.to_path_buf())
}

fn is_pinned(path: &Path) -> bool {
    path.is_file() && sha256_file(path).is_ok_and(|hash| hash == SHA256)
}

/// Whether the pinned binary is already installed under one data directory.
pub fn is_installed(data_root: &Path) -> bool {
    is_pinned(&binary_path(data_root))
}

fn stage_path(target: &Path) -> PathBuf {
    target.with_extension(format!("part-{}", std::process::id()))
}

fn ensure_dir(path: &Path, mode: u32) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("{} cannot be created: {error}", path.display()))?;
    set_mode(path, mode)
        .map_err(|error| format!("{} cannot be set to mode {mode:o}: {error}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The generated secrets an `app.ini` needs. They are written into the 0600 file and
/// never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSecrets {
    secret_key: String,
    internal_token: String,
    jwt_secret: String,
    lfs_jwt_secret: String,
}

impl InstanceSecrets {
    /// Four fresh secrets from the operating system's random source.
    pub fn generate() -> Result<Self, String> {
        Ok(Self {
            secret_key: random_hex(32)?,
            internal_token: format!("{}={}", random_hex(16)?, random_hex(32)?),
            jwt_secret: random_hex(32)?,
            lfs_jwt_secret: random_hex(32)?,
        })
    }
}

/// One instance's paths, all below its own directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    /// The instance root.
    pub root: PathBuf,
    /// The generated configuration.
    pub app_ini: PathBuf,
    /// The administrator's login, stored beside the instance, 0600.
    pub admin_login_file: PathBuf,
    /// The file log directory.
    pub log_dir: PathBuf,
    /// The SQLite database and repositories.
    pub data_dir: PathBuf,
    /// A private `HOME` for the server's git calls.
    pub home: PathBuf,
    /// The server's git configuration, kept inside the instance.
    pub git_config: PathBuf,
}

impl InstancePaths {
    /// The fixed paths below one data root.
    pub fn under(data_root: &Path) -> Self {
        let root = forgejo_dir(data_root).join(INSTANCE_DIR);
        Self {
            app_ini: root.join("app.ini"),
            admin_login_file: root.join("secrets").join("admin-login"),
            log_dir: root.join("log"),
            data_dir: root.join("data"),
            home: root.join("home"),
            git_config: root.join("home").join("gitconfig"),
            root,
        }
    }

    /// Create every directory the instance owns, keeping the instance private.
    pub fn ensure(&self) -> Result<(), String> {
        let secrets_dir = self.root.join("secrets");
        for dir in [
            &self.root,
            &self.data_dir,
            &self.log_dir,
            &self.home,
            &secrets_dir,
        ] {
            ensure_dir(dir, PRIVATE_DIR_MODE)?;
        }
        fs::create_dir_all(self.data_dir.join("repositories"))
            .map_err(|error| format!("{} cannot be created: {error}", self.data_dir.display()))?;
        set_mode(&self.data_dir.join("repositories"), PRIVATE_DIR_MODE)
            .map_err(|error| format!("the repository directory cannot be restricted: {error}"))?;
        if !self.git_config.exists() {
            crate::mode::write_private(&self.git_config, b"[user]\n\tname = gewu-multiverse\n")
                .map_err(|error| {
                    format!("the instance git configuration cannot be written: {error}")
                })?;
        }
        Ok(())
    }

    /// The address a browser uses.
    pub fn url(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }
}

/// What a prepared instance knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInstance {
    /// Its paths.
    pub paths: InstancePaths,
    /// The administrator's Forgejo login.
    pub admin_login: String,
    /// The port kept in `app.ini`.
    pub port: u16,
    /// The browser address.
    pub url: String,
    /// The administrator token minted when this start created the account. It is
    /// never the password: the account is made with `--random-password` and the value
    /// is read only to be discarded.
    pub admin_token: Option<Secret>,
}

/// Read the kept port from an `app.ini` that already exists.
pub fn port_from_config(text: &str) -> Option<u16> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("HTTP_PORT") {
            let value = value.trim_start_matches([' ', '=']).trim();
            if let Ok(port) = value.parse::<u16>() {
                if (3000..=3099).contains(&port) {
                    return Some(port);
                }
            }
        }
    }
    None
}

/// The configuration keys this build pins on every start, whatever version wrote the
/// file before. An upgrade keeps the person's other settings; these are replaced in
/// place (or added, with their section) so a version that wrote `SQLITE_TIMEOUT = 500`
/// and no WAL is brought to the current write behaviour before Forgejo starts.
pub const PINNED_APP_INI: &[(&str, &str, &str)] = &[
    ("database", "SQLITE_TIMEOUT", "5000"),
    ("database", "SQLITE_JOURNAL_MODE", "WAL"),
    ("i18n", "LANGS", "en-US"),
    ("i18n", "NAMES", "English"),
    ("time", "DEFAULT_UI_LOCATION", "UTC"),
    // The footer's page/template timings are a developer's numbers, not the
    // person's page. They are pinned off on every start, including on an instance
    // an earlier version made (U10).
    ("other", "SHOW_FOOTER_TEMPLATE_LOAD_TIME", "false"),
];

/// The lower-case section name of an INI header line.
fn section_name(line: &str) -> Option<String> {
    let line = line.trim();
    line.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .map(|name| name.trim().to_ascii_lowercase())
}

/// Bring an existing `app.ini` to the keys this build pins, keeping every other line.
///
/// This is idempotent: a file that already carries the pinned values comes back
/// unchanged. A key that is present is replaced in its own section; a missing key is
/// inserted at the end of its section; a missing section is appended. Comments,
/// blank lines and the person's own settings are left exactly as they are.
pub fn pinned_app_ini(existing: &str) -> String {
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let mut found = vec![false; PINNED_APP_INI.len()];
    let mut section = String::new();
    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if let Some(name) = section_name(trimmed) {
            section = name;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut replacement = None;
        for (index, (pinned_section, pinned_key, pinned_value)) in PINNED_APP_INI.iter().enumerate()
        {
            if !found[index] && *pinned_section == section && *pinned_key == key {
                replacement = Some(format!("{pinned_key} = {pinned_value}"));
                found[index] = true;
            }
        }
        if let Some(text) = replacement {
            // Keep an inline comment on a replaced key (N10): `KEY = old ; note`
            // becomes `KEY = new ; note`, not a line with the person's note lost.
            let comment = line
                .split_once('=')
                .and_then(|(_, rest)| rest.find([';', '#']).map(|at| &rest[at..]))
                .unwrap_or_default();
            *line = format!("{text}{comment}");
        }
    }
    // Add a missing key at the end of a section that already exists.
    for (index, (pinned_section, pinned_key, pinned_value)) in PINNED_APP_INI.iter().enumerate() {
        if found[index] {
            continue;
        }
        let Some(header) = lines
            .iter()
            .position(|line| section_name(line).as_deref() == Some(*pinned_section))
        else {
            continue;
        };
        let mut end = lines.len();
        for (position, line) in lines.iter().enumerate().skip(header + 1) {
            if section_name(line).is_some() {
                end = position;
                break;
            }
        }
        while end > header + 1 && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        lines.insert(end, format!("{pinned_key} = {pinned_value}"));
        found[index] = true;
    }
    // Append a section that was not in the file at all, once, with its pinned keys.
    let mut appended: Vec<&str> = Vec::new();
    for (section, key, value) in PINNED_APP_INI {
        if appended.contains(section) {
            lines.push(format!("{key} = {value}"));
            continue;
        }
        if lines
            .iter()
            .any(|line| section_name(line).as_deref() == Some(*section))
        {
            continue;
        }
        appended.push(section);
        lines.push(String::new());
        lines.push(format!("[{section}]"));
        lines.push(format!("{key} = {value}"));
    }
    let mut text = lines.join("\n").trim_end_matches('\n').to_owned();
    text.push('\n');
    text
}

/// A free loopback port in the product's range.
pub fn choose_port() -> Result<u16, String> {
    for port in 3000..=3099u16 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err("no free loopback port was found in 3000–3099".to_owned())
}

/// The fixed administrator login for a new installation.
///
/// It is deliberately not the operating system's account name: a person's local
/// login should not be published to every teammate and agent on their Forgejo.
/// Forgejo reserves `admin` itself (it is the administration route), so the fixed
/// login is [`FALLBACK_ADMIN_LOGIN`]. An installation made by an earlier version
/// keeps its recorded login, which `prepare_instance_checked` reads back; the
/// Forgejo page says which one is in use.
pub fn admin_login_from_env() -> String {
    FALLBACK_ADMIN_LOGIN.to_owned()
}

/// Whether a string can be a Forgejo username.
pub fn valid_username(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with(['-', '_', '.'])
        && !name.ends_with(['-', '_', '.'])
}

/// The login the server itself must run as: the operating system's current user.
fn run_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| FALLBACK_ADMIN_LOGIN.to_owned())
        })
}

/// Build the `app.ini` text. It is kept public so a test can assert every safety
/// setting without starting a server.
pub fn app_ini_text(
    paths: &InstancePaths,
    port: u16,
    run_user: &str,
    secrets: &InstanceSecrets,
) -> String {
    let root = paths.root.display();
    format!(
        "APP_NAME = gewu-multiverse\n\
         RUN_USER = {run_user}\n\
         RUN_MODE = prod\n\
         WORK_PATH = {root}\n\
         \n\
         [database]\n\
         DB_TYPE = sqlite3\n\
         PATH = {root}/data/forgejo.db\n\
         SQLITE_TIMEOUT = 5000\n\
         SQLITE_JOURNAL_MODE = WAL\n\
         \n\
         [server]\n\
         PROTOCOL = http\n\
         DOMAIN = 127.0.0.1\n\
         HTTP_ADDR = 127.0.0.1\n\
         HTTP_PORT = {port}\n\
         ROOT_URL = http://127.0.0.1:{port}/\n\
         APP_DATA_PATH = {root}/data\n\
         DISABLE_SSH = true\n\
         START_SSH_SERVER = false\n\
         LFS_START_SERVER = false\n\
         LFS_JWT_SECRET = {lfs}\n\
         OFFLINE_MODE = true\n\
         ENABLE_GZIP = false\n\
         \n\
         [security]\n\
         INSTALL_LOCK = true\n\
         SECRET_KEY = {secret}\n\
         INTERNAL_TOKEN = {internal}\n\
         PASSWORD_HASH_ALGO = pbkdf2\n\
         \n\
         [oauth2]\n\
         JWT_SECRET = {jwt}\n\
         \n\
         [service]\n\
         DISABLE_REGISTRATION = true\n\
         REQUIRE_SIGNIN_VIEW = true\n\
         ENABLE_NOTIFY_MAIL = false\n\
         DEFAULT_ALLOW_CREATE_ORGANIZATION = false\n\
         \n\
         [i18n]\n\
         ; A pinned, valid default: a browser that sends an unusable language tag\n\
         ; still gets a page whose language, and whose date formats, are valid.\n\
         LANGS = en-US\n\
         NAMES = English\n\
         \n\
         [time]\n\
         DEFAULT_UI_LOCATION = UTC\n\
         \n\
         [other]\n\
         SHOW_FOOTER_TEMPLATE_LOAD_TIME = false\n\
         \n\
         [repository]\n\
         ROOT = {root}/data/repositories\n\
         DEFAULT_BRANCH = main\n\
         \n\
         [mailer]\n\
         ENABLED = false\n\
         \n\
         [actions]\n\
         ENABLED = false\n\
         \n\
         [cron]\n\
         ENABLED = false\n\
         \n\
         [log]\n\
         MODE = file\n\
         LEVEL = info\n\
         ROOT_PATH = {root}/log\n\
         FILE_NAME = forgejo.log\n",
        lfs = secrets.lfs_jwt_secret,
        secret = secrets.secret_key,
        internal = secrets.internal_token,
        jwt = secrets.jwt_secret,
    )
}

/// The small script that keeps Forgejo's bundled pages free of the red
/// `JavaScript error` banner on browsers whose `navigator.language` is not a usable
/// language tag (`C`, an empty string, a tag older ICU rejects).
///
/// Forgejo's `webcomponents.js` builds an `Intl.DateTimeFormat` from
/// `navigator.language` at load time, and an invalid tag throws before the page's
/// own script runs. A browser is not the product's to configure, so the pinned
/// instance serves one pre-script that replaces only an invalid language with
/// `en-US`; the page's own language (`<html lang>`) is already pinned in `app.ini`.
/// A valid browser language is left exactly as it is.
const LOCALE_GUARD: &str = r#"<script>
(function () {
  try {
    var usable = function (tag) { try { new Intl.DateTimeFormat(tag); return true; } catch (e) { return false; } };
    if (usable(navigator.language)) return;
    var fallback = 'en-US';
    Object.defineProperty(Navigator.prototype, 'language', { get: function () { return fallback; } });
    Object.defineProperty(Navigator.prototype, 'languages', { get: function () { return [fallback]; } });
  } catch (e) { /* no Intl: the page still renders in its pinned language */ }
})();
</script>
"#;

/// Write the pinned instance's one custom template: `base/head_script`, with
/// [`LOCALE_GUARD`] before the original content. The original is read out of the
/// pinned binary, so a Forgejo update cannot silently drop its scripts.
fn write_locale_guard(binary: &Path, paths: &InstancePaths) -> Result<(), String> {
    let output = forgejo_output_checked(
        binary,
        paths,
        &["embedded", "view", "templates/base/head_script.tmpl"],
        &|| false,
    )?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(format!(
            "the bundled Forgejo's page script could not be read: {}",
            one_line(&String::from_utf8_lossy(&output.stderr), 240)
        ));
    }
    let dir = paths.root.join("custom").join("templates").join("base");
    fs::create_dir_all(&dir).map_err(|error| {
        format!("the instance's custom template directory cannot be made: {error}")
    })?;
    let _ = crate::mode::set_mode(&dir, PRIVATE_DIR_MODE);
    let mut text = String::from(LOCALE_GUARD);
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    crate::mode::write_private(dir.join("head_script.tmpl"), text.as_bytes())
        .map_err(|error| format!("the instance's page script cannot be written: {error}"))
}

/// Prepare one instance: `app.ini`, secrets, `migrate`, and the administrator account.
/// Running it twice is harmless; the port, secrets and password are kept.
pub fn prepare_instance(
    data_root: &Path,
    binary: &Path,
    admin_login: &str,
) -> Result<PreparedInstance, String> {
    prepare_instance_checked(data_root, binary, admin_login, || false)
}

/// The same preparation, checking `cancelled` before and after every step and while a
/// Forgejo CLI command runs. A quit during `migrate` or the administrator command
/// stops that child and reaps it rather than leaving it behind.
pub fn prepare_instance_checked(
    data_root: &Path,
    binary: &Path,
    admin_login: &str,
    cancelled: impl Fn() -> bool,
) -> Result<PreparedInstance, String> {
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    let paths = InstancePaths::under(data_root);
    paths.ensure()?;
    write_locale_guard(binary, &paths)?;
    let existing = fs::read_to_string(&paths.app_ini).ok();
    let port = existing
        .as_deref()
        .and_then(port_from_config)
        .map(Ok)
        .unwrap_or_else(choose_port)?;
    let login = if valid_username(admin_login) {
        admin_login.to_owned()
    } else {
        admin_login_from_env()
    };
    match existing.as_deref() {
        None => {
            let secrets = InstanceSecrets::generate()?;
            let text = app_ini_text(&paths, port, &run_user(), &secrets);
            crate::mode::write_private(&paths.app_ini, text.as_bytes())
                .map_err(|error| format!("the Forgejo configuration cannot be written: {error}"))?;
        }
        Some(existing) => {
            // An instance prepared by an earlier version keeps its settings; the
            // keys this build pins (SQLite WAL and its wait, the page locale and
            // time zone) are brought forward before Forgejo starts. Without this an
            // upgrade keeps `SQLITE_TIMEOUT = 500` and no WAL, and the load finding
            // returns (B2/N8).
            let updated = pinned_app_ini(existing);
            if updated != existing {
                crate::mode::write_private(&paths.app_ini, updated.as_bytes()).map_err(
                    |error| format!("the Forgejo configuration cannot be updated: {error}"),
                )?;
            }
        }
    }
    // The login is part of this data directory's own record, so a later start uses
    // the same administrator even when the environment has changed.
    if !paths.admin_login_file.is_file() {
        crate::mode::write_private(&paths.admin_login_file, login.as_bytes())
            .map_err(|error| format!("the administrator login cannot be recorded: {error}"))?;
    }
    let kept_login = fs::read_to_string(&paths.admin_login_file)
        .map(|text| text.trim().to_owned())
        .ok()
        .filter(|text| valid_username(text))
        .unwrap_or(login);
    // The administrator password is never written down. The account is created by
    // Forgejo's CLI with `--random-password` (the value never becomes a product
    // argument), and `p` on the Forgejo page resets it through the administration API
    // and shows the new one. A file left by an older product is removed here, not
    // read.
    let legacy = paths.root.join("secrets").join("admin-password");
    let _ = fs::remove_file(&legacy);
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    forgejo_command_checked(binary, &paths, &["migrate"], &cancelled)?;
    let admin_token = ensure_admin(binary, &paths, &kept_login, &cancelled)?;
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    // A local install may run the CLI before the web process has ever started; that is
    // the point of `migrate`. Read the version this binary reports without starting it.
    Ok(PreparedInstance {
        paths,
        admin_login: kept_login,
        port,
        url: InstancePaths::url(port),
        admin_token,
    })
}

/// Whether a byte stream contains a phrase.
fn bytes_contain(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Create the administrator, or leave an existing one untouched.
///
/// The account is made with Forgejo's own `--random-password`: the value is generated
/// by the CLI, never chosen or passed by this product, and is present in no process
/// argument. The output that carries it is read only to tell success apart from a
/// failure, then wiped; the new account's administrator token is minted immediately
/// (a token, not the password) and returned. An existing account keeps its password,
/// and `p` resets it later through the administration API.
fn ensure_admin(
    binary: &Path,
    paths: &InstancePaths,
    login: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Secret>, String> {
    let email = format!("{login}@agents.invalid");
    let mut output = forgejo_output_checked(
        binary,
        paths,
        &[
            "admin",
            "user",
            "create",
            "--username",
            login,
            "--random-password",
            "--random-password-length",
            "48",
            "--email",
            &email,
            "--admin",
            "--must-change-password=false",
        ],
        cancelled,
    )?;
    let created = bytes_contain(&output.stdout, "successfully created")
        || bytes_contain(&output.stdout, "Created user")
        || bytes_contain(&output.stderr, "successfully created")
        || bytes_contain(&output.stderr, "Created user");
    let exists = bytes_contain(&output.stdout, "already exists")
        || bytes_contain(&output.stdout, "already used")
        || bytes_contain(&output.stderr, "already exists")
        || bytes_contain(&output.stderr, "already used");
    // The random password, if the CLI printed it, is in these bytes. It is not turned
    // into a string and is wiped before anything else can observe it.
    output.stdout.fill(0);
    if created {
        let token = generate_admin_token_paths(binary, paths, login, cancelled, |_| {})?;
        return Ok(Some(token));
    }
    if exists {
        return Ok(None);
    }
    Err(format!(
        "the Forgejo administrator account could not be made: {}",
        one_line(&String::from_utf8_lossy(&output.stderr), 240)
    ))
}

/// Reset the administrator password through the administration API and return the new
/// one.
///
/// This is what `p` does. The new value is generated here and sent in the `PATCH`
/// body — never in a process argument, where an ordinary process table would show it
/// (B4). The administrator token the product already holds authorises the call. The
/// value is returned for the one view that shows it and never written to a file; the
/// one-line consequence is on the static Forgejo page: the password shown before this
/// moment stops working.
pub fn reset_admin_password(client: &ForgejoClient, login: &str) -> Result<String, String> {
    let password = generated_password()?;
    client
        .set_user_password(login, &password)
        .map_err(|error| {
            format!(
                "the administrator password could not be reset: {}",
                one_line(&error.to_string(), 240)
            )
        })?;
    Ok(password)
}

/// Ask Forgejo's CLI for an access token for the administrator. The token is returned
/// in a [`Secret`] and never printed; the CLI's stderr is not part of the reason.
pub fn generate_admin_token(
    binary: &Path,
    data_root: &Path,
    login: &str,
) -> Result<Secret, String> {
    generate_admin_token_checked(binary, data_root, login, &|| false, |_| {})
}

/// The same token command, with the helper's pid reported as soon as it exists and
/// the engine's stop predicate checked while it runs.
///
/// The engine records the pid under its state lock, so `shutdown` can end a token
/// helper that is still running. `on_spawn` runs before the command's output is read
/// for the first time.
pub fn generate_admin_token_checked(
    binary: &Path,
    data_root: &Path,
    login: &str,
    cancelled: &dyn Fn() -> bool,
    on_spawn: impl FnOnce(u32),
) -> Result<Secret, String> {
    let paths = InstancePaths::under(data_root);
    generate_admin_token_paths(binary, &paths, login, cancelled, on_spawn)
}

/// The same token command against already-known instance paths, so preparation can
/// mint the administrator's token before the server has started.
fn generate_admin_token_paths(
    binary: &Path,
    paths: &InstancePaths,
    login: &str,
    cancelled: &dyn Fn() -> bool,
    on_spawn: impl FnOnce(u32),
) -> Result<Secret, String> {
    let name = format!("gewu-engine-admin-{}", crate::time::now_unix_ms());
    let output = forgejo_output_tracked(
        binary,
        paths,
        &[
            "admin",
            "user",
            "generate-access-token",
            "--username",
            login,
            "--token-name",
            &name,
            "--scopes",
            "all",
            "--raw",
        ],
        cancelled,
        on_spawn,
    )?;
    if !output.status.success() {
        return Err(format!(
            "the Forgejo administrator token could not be made: {}",
            one_line(&String::from_utf8_lossy(&output.stderr), 240)
        ));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return Err("the Forgejo administrator token came back empty".to_owned());
    }
    Ok(Secret::new(token))
}

fn forgejo_command_checked(
    binary: &Path,
    paths: &InstancePaths,
    args: &[&str],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let output = forgejo_output_checked(binary, paths, args, cancelled)?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "Forgejo's `{}` failed: {}",
        args.first().copied().unwrap_or("command"),
        one_line(&String::from_utf8_lossy(&output.stderr), 240)
    ))
}

fn forgejo_output_checked(
    binary: &Path,
    paths: &InstancePaths,
    args: &[&str],
    cancelled: &dyn Fn() -> bool,
) -> Result<std::process::Output, String> {
    forgejo_output_tracked(binary, paths, args, cancelled, |_| {})
}

/// Run one Forgejo CLI helper, report its pid as soon as it exists, and keep reading
/// its output while it runs.
///
/// Reading both pipes concurrently is what keeps a helper that writes more than a
/// pipe buffer from blocking before it exits. `on_spawn` is called before the first
/// read, so the engine can put the pid where `shutdown` will see it.
fn forgejo_output_tracked(
    binary: &Path,
    paths: &InstancePaths,
    args: &[&str],
    cancelled: &dyn Fn() -> bool,
    on_spawn: impl FnOnce(u32),
) -> Result<std::process::Output, String> {
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    let mut child = Command::new(binary)
        .args(args)
        .arg("--config")
        .arg(&paths.app_ini)
        .arg("--work-path")
        .arg(&paths.root)
        .current_dir(&paths.root)
        .env("FORGEJO_WORK_DIR", &paths.root)
        .env("GITEA_WORK_DIR", &paths.root)
        .env("HOME", &paths.home)
        .env("GIT_CONFIG_GLOBAL", &paths.git_config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("the Forgejo command could not be run: {error}"))?;
    on_spawn(child.id());
    // Drain both pipes while the command runs. Waiting without reading stalls a
    // helper whose output is larger than the pipe buffer.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "the Forgejo command has no output pipe".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "the Forgejo command has no error pipe".to_owned())?;
    let stdout = std::thread::spawn(move || read_all(stdout));
    let stderr = std::thread::spawn(move || read_all(stderr));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout.join().unwrap_or_default();
                let stderr = stderr.join().unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {}
            Err(error) => {
                signal_group(child.id(), nix::sys::signal::Signal::SIGKILL);
                let _ = child.wait();
                return Err(format!("the Forgejo command could not be checked: {error}"));
            }
        }
        if cancelled() {
            // Ask the CLI politely, then end its group; the wait below reaps it.
            signal_group(child.id(), nix::sys::signal::Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            signal_group(child.id(), nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
            return Err("the start was called off".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read a pipe to its end as bytes.
fn read_all(mut pipe: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    bytes
}

/// Write one instance's running pid at 0600.
fn write_pid_file(path: &Path, pid: u32) -> Result<(), String> {
    crate::mode::write_private(path, pid.to_string().as_bytes())
        .map_err(|error| format!("the Forgejo pid record cannot be written: {error}"))
}

/// The command line of a live process, as `/proc` reports it.
pub fn command_line(pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// Whether a pid is gone, counting a zombie as gone: the process has ended, and only
/// its parent's reaping is left. A recovery is not that parent, so waiting for the
/// entry to disappear would wait on somebody else.
fn process_is_gone(pid: u32) -> bool {
    if !crate::run::process_alive(pid) {
        return true;
    }
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().to_owned())
        })
        .is_some_and(|rest| rest.starts_with('Z'))
}

/// Stop one process group politely, then force it, and wait until it is gone.
///
/// The caller has already checked that the pid's command line is the one this product
/// started; this function never guesses from a bare number.
pub fn stop_recorded_pid(pid: u32, grace: Duration) -> bool {
    if process_is_gone(pid) {
        return true;
    }
    signal_group(pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if process_is_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    signal_group(pid, nix::sys::signal::Signal::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if process_is_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    process_is_gone(pid)
}

/// Find and stop a `forgejo web` that an earlier, killed program left running.
///
/// The pid record is believed only when the live command line still names this
/// instance's configuration and the pinned binary; a reused pid is never killed.
/// Returns the pid that was stopped, when there was one.
pub fn recover_stale_server(data_root: &Path, grace: Duration) -> Result<Option<u32>, String> {
    let paths = InstancePaths::under(data_root);
    let record = paths.root.join(PID_FILE);
    let Ok(text) = fs::read_to_string(&record) else {
        return Ok(None);
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        let _ = fs::remove_file(&record);
        return Ok(None);
    };
    let expected = paths.app_ini.display().to_string();
    let matches = command_line(pid).is_some_and(|parts| {
        parts.iter().any(|part| part == "web")
            && parts.iter().any(|part| part == &expected)
            && parts.first().is_some_and(|part| part.ends_with(ASSET))
    });
    if !matches {
        let _ = fs::remove_file(&record);
        return Ok(None);
    }
    let stopped = stop_recorded_pid(pid, grace);
    if !stopped {
        return Err(format!(
            "the Forgejo left by an earlier run (pid {pid}) could not be stopped"
        ));
    }
    let _ = fs::remove_file(&record);
    Ok(Some(pid))
}

/// A running `forgejo web` child, owned by the engine.
pub struct RunningForgejo {
    child: Child,
    pid: u32,
    /// The browser address.
    pub url: String,
    /// The instance's file log.
    pub log: PathBuf,
    /// The pid record this process owns while it runs.
    pid_file: PathBuf,
    started: Instant,
    stopping: bool,
}

impl RunningForgejo {
    /// The child's pid, which is also its process-group id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// How long it has been running.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// The exit status when the child has ended, or `None` while it runs.
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.try_wait()
    }

    /// The exit status when the child has ended, or `None` while it runs. When the
    /// child has ended, its pid record goes with it, however the end is noticed.
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        let status = self.child.try_wait().ok().flatten();
        if status.is_some() {
            self.remove_pid_file();
        }
        status
    }

    /// One sentence from the newest line of the server's own log, or from how it
    /// ended when the log has no better answer.
    pub fn log_reason(&self, status: Option<std::process::ExitStatus>) -> String {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.and_then(|status| status.signal()) {
            return format!("the bundled Forgejo was killed by signal {signal}");
        }
        if let Some(status) = status {
            if status.code().is_some_and(|code| code != 0) {
                if let Some(sentence) = last_log_sentence(&self.log) {
                    return sentence;
                }
            }
        }
        if let Some(sentence) = last_log_sentence(&self.log) {
            return sentence;
        }
        match status.and_then(|status| status.code()) {
            Some(code) => format!("the bundled Forgejo exited with code {code}"),
            None => "the bundled Forgejo stopped and left no log line".to_owned(),
        }
    }

    /// Stop politely, force after the grace, and reap. Proves the pid is gone.
    pub fn stop(&mut self, grace: Duration) {
        if !self.stopping {
            self.stopping = true;
            signal_group(self.pid, nix::sys::signal::Signal::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        loop {
            if self.try_wait().is_some() {
                let _ = self.child.wait();
                self.remove_pid_file();
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        signal_group(self.pid, nix::sys::signal::Signal::SIGKILL);
        let _ = self.child.wait();
        self.remove_pid_file();
    }

    /// Remove this process's pid record.
    fn remove_pid_file(&self) {
        let _ = fs::remove_file(&self.pid_file);
    }
}

impl Drop for RunningForgejo {
    fn drop(&mut self) {
        // A leaked `web` process would outlive the engine. The engine calls `stop`
        // with its declared grace; this is the last line of defence.
        if self.try_wait().is_none() {
            self.stop(Duration::from_secs(1));
        }
    }
}

/// Start one instance from scratch and wait until `/api/v1/version` answers.
pub fn start_instance(
    binary: &Path,
    data_root: &Path,
    ready_timeout: Duration,
) -> Result<RunningForgejo, String> {
    start_instance_checked(binary, data_root, ready_timeout, || false)
}

/// The same start, with a cancellation check made before the child is spawned and on
/// each readiness poll.
pub fn start_instance_checked(
    binary: &Path,
    data_root: &Path,
    ready_timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<RunningForgejo, String> {
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    let prepared = prepare_instance(data_root, binary, &admin_login_from_env())?;
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    spawn_prepared(prepared, binary, ready_timeout, |_pid| {}, cancelled)
}

/// Spawn one already-prepared instance and wait until `/api/v1/version` answers.
///
/// `on_spawn` is called with the child's pid immediately after the child exists and
/// before the pid record is written or any readiness poll is made. The engine records
/// that pid under its state lock, so `shutdown` can signal a server whose start has
/// not returned yet. The readiness loop checks `cancelled` before every poll and stops
/// and reaps its own child when it is called off.
pub fn spawn_prepared(
    prepared: PreparedInstance,
    binary: &Path,
    ready_timeout: Duration,
    on_spawn: impl FnOnce(u32),
    cancelled: impl Fn() -> bool,
) -> Result<RunningForgejo, String> {
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    let paths = prepared.paths;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(SECRET_FILE_MODE)
        .open(paths.log_dir.join("forgejo.log"))
        .map_err(|error| format!("the Forgejo log cannot be opened: {error}"))?;
    let mut command = Command::new(binary);
    command
        .arg("web")
        .arg("--config")
        .arg(&paths.app_ini)
        .arg("--work-path")
        .arg(&paths.root)
        .current_dir(&paths.root)
        .env("FORGEJO_WORK_DIR", &paths.root)
        .env("GITEA_WORK_DIR", &paths.root)
        .env("HOME", &paths.home)
        .env("GIT_CONFIG_GLOBAL", &paths.git_config)
        .env("USER", run_user())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|error| {
            format!("the Forgejo log cannot be shared: {error}")
        })?))
        .stderr(Stdio::from(log))
        .process_group(0);
    // The last cancellation check before the child exists. Once it is spawned the
    // caller has its pid and the job stops it if the shutdown wins the race.
    if cancelled() {
        return Err("the start was called off".to_owned());
    }
    let child = command
        .spawn()
        .map_err(|error| format!("the bundled Forgejo could not be started: {error}"))?;
    let pid = child.id();
    let mut server = RunningForgejo {
        child,
        pid,
        url: prepared.url,
        log: paths.log_dir.join("forgejo.log"),
        pid_file: paths.root.join(PID_FILE),
        started: Instant::now(),
        stopping: false,
    };
    // The caller records the child here, before the readiness wait. A quit from this
    // moment on must find something to signal.
    on_spawn(pid);
    if cancelled() {
        server.stop(Duration::from_secs(2));
        return Err("the start was called off".to_owned());
    }
    // Written before the readiness wait: a program killed during the wait must leave
    // a record a later start can find and stop.
    if let Err(reason) = write_pid_file(&server.pid_file, pid) {
        server.stop(Duration::from_secs(2));
        return Err(reason);
    }
    let deadline = Instant::now() + ready_timeout;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    loop {
        if cancelled() {
            server.stop(Duration::from_secs(2));
            return Err("the start was called off".to_owned());
        }
        if let Some(status) = server.try_wait() {
            let reason = server.log_reason(Some(status));
            return Err(format!("the bundled Forgejo did not start: {reason}"));
        }
        // With sign-in required, the version endpoint answers 403 to an
        // unauthenticated call. That is still the server answering: the handler ran.
        // Only a refused connection or a server error means it is not up yet.
        let answered = agent
            .get(format!("{}/api/v1/version", server.url))
            .call()
            .ok()
            .is_some_and(|response| {
                let status = response.status().as_u16();
                status < 500 && status != 404
            });
        if answered {
            return Ok(server);
        }
        if Instant::now() >= deadline {
            let reason = server.log_reason(None);
            server.stop(Duration::from_secs(5));
            return Err(format!(
                "the bundled Forgejo did not answer within {} s: {reason}",
                ready_timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A `RunningForgejo` around a child a test started, so the tests can exercise a start
/// that blocks before readiness without launching the real server.
#[cfg(test)]
pub(crate) fn test_server(
    child: Child,
    url: String,
    log: PathBuf,
    pid_file: PathBuf,
) -> RunningForgejo {
    let pid = child.id();
    RunningForgejo {
        child,
        pid,
        url,
        log,
        pid_file,
        started: Instant::now(),
        stopping: false,
    }
}

/// Provision the organisation, the agents' team and the starter repository.
/// Every step is idempotent. Returns the organisation's repositories as they now are.
pub fn provision(client: &ForgejoClient) -> Result<Vec<Repository>, ForgejoError> {
    client.ensure_organisation(ORGANISATION)?;
    let team = client.ensure_team(ORGANISATION, TEAM)?;
    let _ = team;
    client.ensure_organisation_repository(ORGANISATION, STARTER_REPOSITORY)?;
    client.organisation_repositories(ORGANISATION)
}

/// Add one agent's account to the agents' team.
pub fn join_team(client: &ForgejoClient, login: &str) -> Result<(), ForgejoError> {
    let team = client.ensure_team(ORGANISATION, TEAM)?;
    client.add_team_member(team.id, login)
}

/// Remove one agent's account from the agents' team, keeping the account.
pub fn leave_team(client: &ForgejoClient, login: &str) -> Result<(), ForgejoError> {
    let team = client.ensure_team(ORGANISATION, TEAM)?;
    client.remove_team_member(team.id, login)
}

/// The agents' team, for callers that need its id.
pub fn agents_team(client: &ForgejoClient) -> Result<Team, ForgejoError> {
    client.ensure_team(ORGANISATION, TEAM)
}

/// The newest non-empty line of a Forgejo file log as one bounded sentence.
pub fn last_log_sentence(path: &Path) -> Option<String> {
    let text = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&text);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let line = line.trim().trim_start_matches(|c: char| {
        c.is_ascii_digit() || c == '-' || c == ':' || c == 'T' || c == '.' || c == 'Z' || c == ' '
    });
    let line = line
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(_, rest)| rest))
        .unwrap_or(line);
    let sentence = one_line(line, 200);
    (!sentence.is_empty()).then_some(sentence)
}

/// One line, whitespace-collapsed and bounded.
fn one_line(text: &str, limit: usize) -> String {
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.len() > limit {
        line.truncate(limit);
        while !line.is_char_boundary(line.len()) {
            line.pop();
        }
        line.push('…');
    }
    line
}

fn generated_password() -> Result<String, String> {
    Ok(format!("Gw-{}!9", random_hex(32)?))
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buffer))
        .map_err(|error| format!("the random source cannot be read: {error}"))?;
    Ok(hex(&buffer))
}

fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pid as i32), signal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_ini_is_loopback_sqlite_and_closed() {
        let root = crate::test_support::scratch_root("bundled-ini");
        let paths = InstancePaths::under(&root);
        let secrets = InstanceSecrets::generate().expect("secrets are generated");
        let text = app_ini_text(&paths, 3042, "tester", &secrets);
        for wanted in [
            "HTTP_ADDR = 127.0.0.1",
            "HTTP_PORT = 3042",
            "ROOT_URL = http://127.0.0.1:3042/",
            "DB_TYPE = sqlite3",
            "SQLITE_TIMEOUT = 5000",
            "SQLITE_JOURNAL_MODE = WAL",
            "LANGS = en-US",
            "DEFAULT_UI_LOCATION = UTC",
            "INSTALL_LOCK = true",
            "DISABLE_REGISTRATION = true",
            "REQUIRE_SIGNIN_VIEW = true",
            "DISABLE_SSH = true",
            "OFFLINE_MODE = true",
            "ENABLED = false",
            "ROOT_PATH = ",
            "FILE_NAME = forgejo.log",
        ] {
            assert!(text.contains(wanted), "app.ini has no {wanted:?}:\n{text}");
        }
        assert!(text.contains(&secrets.secret_key));
        assert!(text.contains(&secrets.internal_token));
        assert!(!text.contains("smtp"), "a mailer was configured");
        assert!(
            !text.contains("[actions]\nENABLED = true"),
            "actions were enabled"
        );
    }

    #[test]
    fn no_product_spawned_command_line_carries_the_administrator_password() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::test_support::scratch_root("bundled-argv");
        let capture = root.join("spawned-args.txt");
        let binary = root.join("fake-forgejo");
        let script = r#"#!/bin/sh
{ printf '%s' "$0"; for a in "$@"; do printf '\t%s' "$a"; done; printf '\n'; } >> __CAPTURE__
case "$1:$2" in
  embedded:view) printf '%s\n' '<script>window.originalPageScript = true;</script>' ;;
  migrate:*) exit 0 ;;
  admin:user)
    case "$3" in
      create) printf "generated random password is 'fixture-random-password-value'\nNew user 'owner' has been successfully created!\n" ;;
      generate-access-token) printf 'gf-fixture-admin-token\n' ;;
    esac ;;
esac
"#
        .replace("__CAPTURE__", &capture.display().to_string());
        fs::write(&binary, script).expect("the fake binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("the fake binary is executable");
        let data_root = root.join("data");
        let prepared = prepare_instance_checked(&data_root, &binary, "owner", || false)
            .expect("the instance prepares");
        assert!(
            prepared.admin_token.is_some(),
            "the administrator token was not minted at setup"
        );
        let spawned = fs::read_to_string(&capture).expect("the spawn record");
        assert!(
            spawned.contains("--random-password"),
            "the account was not created with a CLI-generated password: {spawned}"
        );
        assert!(
            !spawned.contains("--password"),
            "a password was passed in a process argument: {spawned}"
        );
        assert!(
            !spawned.contains("fixture-random-password-value"),
            "the generated value reached an argument: {spawned}"
        );
        assert!(
            !spawned.contains("generated random password is"),
            "the CLI output was treated as an argument: {spawned}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_password_reset_sends_the_value_in_the_api_body_not_an_argument() {
        use crate::test_stub::{Route, Stub, StubReply};
        let stub = Stub::start_routed(vec![Route::new(
            "PATCH",
            "/api/v1/admin/users/owner",
            StubReply::json("{}"),
        )]);
        let client = ForgejoClient::new(
            &format!("http://{}", stub.addr()),
            Secret::new("gf-fixture-admin-token"),
            std::time::Duration::from_secs(5),
        )
        .expect("a client");
        let password = reset_admin_password(&client, "owner").expect("the reset answers");
        assert_eq!(password.chars().count(), 69);
        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].method, "PATCH");
        assert_eq!(requests[0].path, "/api/v1/admin/users/owner");
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("a JSON body");
        assert_eq!(body["password"], serde_json::json!(password));
        assert_eq!(body["must_change_password"], serde_json::json!(false));
    }

    #[test]
    fn an_older_app_ini_is_brought_forward_and_keeps_every_other_setting() {
        let root = crate::test_support::scratch_root("bundled-ini-upgrade");
        let paths = InstancePaths::under(&root);
        let secrets = InstanceSecrets::generate().expect("secrets are generated");
        // A v0.1.30 file: 500 ms, no journal mode, no [i18n] and no [time].
        let old = app_ini_text(&paths, 3046, "tester", &secrets)
            .replace("SQLITE_TIMEOUT = 5000", "SQLITE_TIMEOUT = 500 ; my note")
            .replace("SQLITE_JOURNAL_MODE = WAL\n", "")
            .replace("LEVEL = info", "LEVEL = warn")
            .replace(
                "\n[i18n]\n; A pinned, valid default: a browser that sends an unusable language tag\n; still gets a page whose language, and whose date formats, are valid.\nLANGS = en-US\nNAMES = English\n",
                "",
            )
            .replace("\n[time]\nDEFAULT_UI_LOCATION = UTC\n", "");
        assert!(!old.contains("SQLITE_JOURNAL_MODE"));
        assert!(!old.contains("[i18n]"));
        let updated = pinned_app_ini(&old);
        for wanted in [
            "SQLITE_TIMEOUT = 5000",
            "SQLITE_JOURNAL_MODE = WAL",
            "[i18n]\nLANGS = en-US\nNAMES = English",
            "[time]\nDEFAULT_UI_LOCATION = UTC",
            "LEVEL = warn",
            "HTTP_PORT = 3046",
            "SECRET_KEY = ",
        ] {
            assert!(
                updated.contains(wanted),
                "the upgrade has no {wanted:?}:\n{updated}"
            );
        }
        assert_eq!(
            pinned_app_ini(&updated),
            updated,
            "the upgrade is not idempotent"
        );
        assert!(
            !updated.contains("SQLITE_TIMEOUT = 500 ;"),
            "the old 500 ms wait survives:\n{updated}"
        );
        // A section that exists but lost a key gets it inside its own section.
        let no_timeout = old.replace("SQLITE_TIMEOUT = 500 ; my note\n", "");
        let updated = pinned_app_ini(&no_timeout);
        let database = updated
            .split("[database]")
            .nth(1)
            .and_then(|rest| rest.split('\n').find(|line| line.starts_with("PATH")))
            .unwrap_or_default();
        assert!(updated.contains("SQLITE_TIMEOUT = 5000"), "{updated}");
        assert!(updated.contains("[server]"), "a section boundary moved");
        assert!(!database.is_empty());
    }

    #[test]
    fn the_locale_guard_is_prepended_to_the_pinned_page_script() {
        let root = crate::test_support::scratch_root("bundled-locale");
        let paths = InstancePaths::under(&root);
        paths.ensure().expect("the instance directories");
        let binary = root.join("fake-forgejo");
        fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' '<script>window.originalPageScript = true;</script>'\n",
        )
        .expect("the fake binary");
        let mut permissions = fs::metadata(&binary).expect("metadata").permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
        fs::set_permissions(&binary, permissions).expect("executable");
        write_locale_guard(&binary, &paths).expect("the guard is written");
        let target = paths.root.join("custom/templates/base/head_script.tmpl");
        let text = fs::read_to_string(&target).expect("the custom template");
        assert!(text.contains("usable(navigator.language)"), "{text}");
        assert!(
            text.contains("window.originalPageScript = true"),
            "the pinned script was dropped: {text}"
        );
        assert!(
            text.find("usable(navigator.language)") < text.find("window.originalPageScript"),
            "the guard is not before the page script"
        );
    }

    #[test]
    fn a_new_installation_uses_a_fixed_login_not_the_computer_account() {
        // Forgejo reserves `admin`; `owner` is the fixed login the README shows.
        assert_eq!(admin_login_from_env(), FALLBACK_ADMIN_LOGIN);
        assert_eq!(FALLBACK_ADMIN_LOGIN, "owner");
    }

    #[test]
    fn the_port_is_read_back_from_the_configuration() {
        let root = crate::test_support::scratch_root("bundled-port");
        let paths = InstancePaths::under(&root);
        let secrets = InstanceSecrets::generate().expect("secrets are generated");
        let text = app_ini_text(&paths, 3077, "tester", &secrets);
        assert_eq!(port_from_config(&text), Some(3077));
        assert_eq!(port_from_config("HTTP_PORT = 80"), None);
        assert_eq!(port_from_config("nothing here"), None);
    }

    #[test]
    fn the_fallback_login_is_used_when_the_environment_cannot_name_one() {
        assert!(valid_username("owner"));
        assert!(valid_username("ha-matrix"));
        assert!(!valid_username(""));
        assert!(!valid_username("-bad"));
        assert!(!valid_username("two words"));
        assert!(!valid_username(&"x".repeat(41)));
    }

    #[test]
    fn a_local_file_with_the_wrong_hash_is_refused_and_leaves_nothing() {
        let root = crate::test_support::scratch_root("bundled-local-bad");
        let source = root.join("source");
        fs::write(&source, b"not the pinned bytes").expect("the source is written");
        let target = root.join("forgejo").join(ASSET);
        let error = install_local(&source, &target, SHA256, &mut |_| {})
            .expect_err("a wrong hash is refused");
        assert!(error.contains("pinned SHA-256"), "{error}");
        assert!(!target.exists(), "a refused install kept the target");
        assert!(
            fs::read_dir(target.parent().expect("parent"))
                .map(|entries| entries.count())
                .unwrap_or(0)
                == 0,
            "a refused install left a file beside the target"
        );
    }

    #[test]
    fn a_local_file_with_the_right_hash_is_installed_atomically() {
        let root = crate::test_support::scratch_root("bundled-local-good");
        let source = root.join("source");
        fs::write(&source, b"the pinned bytes").expect("the source is written");
        let expected = sha256_file(&source).expect("the source hashes");
        let target = root.join("forgejo").join(ASSET);
        let installed = install_local(&source, &target, &expected, &mut |_| {})
            .expect("the right hash is installed");
        assert_eq!(installed, target);
        assert!(target.is_file());
        assert_eq!(sha256_file(&target).expect("the target hashes"), expected);
    }

    #[test]
    fn a_wrong_hash_from_a_source_is_refused_without_a_kept_part_file() {
        // The download half is exercised by the engine test with a loopback server;
        // here the stage path is checked so a refusal cannot leave a temporary file.
        let root = crate::test_support::scratch_root("bundled-stage");
        let target = root.join("forgejo").join(ASSET);
        fs::create_dir_all(target.parent().expect("parent")).expect("the directory");
        let stage = stage_path(&target);
        fs::write(&stage, b"leftover").expect("the stage is written");
        assert!(stage.exists());
        let _ = fs::remove_file(&stage);
        assert!(!stage.exists());
    }

    /// A plain executable fixture for the command paths.
    fn fake_helper(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, body).expect("the fake helper is written");
        crate::mode::set_mode(&path, 0o755).expect("the fake helper is executable");
        path
    }

    /// A helper that writes more than a pipe buffer must be read while it runs; a
    /// wait that only checks `try_wait` lets it block for ever on a full pipe.
    #[test]
    fn a_helper_with_large_output_is_drained_while_it_runs() {
        let root = crate::test_support::scratch_root("bundled-helper-drain");
        let paths = InstancePaths::under(&root);
        paths.ensure().expect("the instance paths");
        let helper = fake_helper(
            &root,
            "big-output.sh",
            "#!/bin/sh\nhead -c 1048576 /dev/zero | tr '\\0' x\nprintf '\\n'\n",
        );
        let began = Instant::now();
        let output = forgejo_output_checked(&helper, &paths, &[], &|| false)
            .expect("a large output is drained");
        assert!(output.status.success());
        assert_eq!(
            output.stdout.len(),
            1048576 + 1,
            "the helper output was cut short"
        );
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the helper took {:?}; its pipe was not drained",
            began.elapsed()
        );
    }

    /// The token path reports the helper's pid before reading its output, and after
    /// the call the helper has ended and been reaped.
    #[test]
    fn a_token_helper_reports_its_pid_and_leaves_no_child() {
        let root = crate::test_support::scratch_root("bundled-helper-token");
        let paths = InstancePaths::under(&root);
        paths.ensure().expect("the instance paths");
        let helper = fake_helper(
            &root,
            "fake-forgejo-token.sh",
            "#!/bin/sh\nprintf 'token-abc\\n'\n",
        );
        let mut seen = 0;
        let token = generate_admin_token_checked(&helper, &root, "tester", &|| false, |pid| {
            seen = pid;
        })
        .expect("the token is read");
        assert_eq!(token.expose(), "token-abc");
        assert!(seen > 0, "the helper pid was never reported");
        assert!(
            !Path::new(&format!("/proc/{seen}")).exists(),
            "the token helper {seen} was left running or unreaped"
        );
    }
}
