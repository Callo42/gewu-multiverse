//! The one credentials file, and the one secret it hands out.
//!
//! This file is the only place a secret lives. The tool does four things with it and
//! nothing else:
//!
//! 1. create it, with mode 0600, when it is not there yet;
//! 2. tighten its mode to 0600 if it is wider than that;
//! 3. read back the mode, so the mode shown on the Credentials screen is the mode on
//!    disk rather than an assumption;
//! 4. read one secret out of it — an agent's model key — at the moment of a model
//!    call.
//!
//! A secret read here is a [`Secret`], and a `Secret` cannot be printed by accident:
//! its `Debug` and `Display` write `<redacted>`, and the only way to reach the value
//! is [`Secret::expose`], which is called in exactly one place (the request header).
//! Every string that is written to a transcript, a log or an error is passed through
//! [`Secret::redact`], so the key cannot be recovered from anything the tool leaves
//! behind. A run wraps its model key in [`Secret::with_extra_redactions`] carrying
//! [`all_secret_values`] — every token and every other agent's key in the file — so
//! not even a tool that printed the credentials file can carry a secret into a log or
//! a transcript.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::mode::{PRIVATE_DIR_MODE, SECRET_FILE_MODE, mode_of, render, set_mode};

/// The text that stands in for a secret wherever one would otherwise be written.
pub const REDACTED: &str = "<redacted>";

/// The text a fresh credentials file starts with. It holds no secret: the keys are
/// there, the values are empty, and the comments say what the file is for.
pub const TEMPLATE: &str = r#"# gewu-multiverse credentials
#
# Secrets only. This file is created with mode 0600: readable and writable by you
# alone. It is never printed, never written to a log, and never passed on a command
# line. The configuration file holds no secret at all.
#
# The token the tool uses for the role check: the repositories it lists are the
# repositories this token can see. Put a member's token here for an ordinary view,
# or an administrator's token when the configuration says role = "admin".
forgejo_token = ""
#
# An administrator's token, if you have one. With it, the tool creates and manages
# one Forgejo account and token per agent, derived from the agent's name. Without
# it, supply each agent's token under [forgejo_agent_tokens] below.
forgejo_admin_token = ""

[model_keys]
# One key per agent name. For an agent called example-agent:
# "example-agent" = ""

[forgejo_agent_tokens]
# One token per agent name, used when no administrator token is configured above:
# "example-agent" = ""
"#;

/// The credentials file, as far as this skeleton is concerned: its path and its mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialsFile {
    path: PathBuf,
    mode: u32,
}

/// Why the credentials file could not be prepared or trusted.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    /// The file exists but is not a regular file.
    #[error("{path} is not a regular file; the credentials file must be a plain file")]
    NotAFile {
        /// The path that was refused.
        path: PathBuf,
    },
    /// A directory on the way could not be created.
    #[error("the directory {path} cannot be created: {source}")]
    Parent {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file could not be created.
    #[error("{path} cannot be created: {source}")]
    Create {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file is not there.
    ///
    /// This is deliberately *not* [`CredentialsError::Create`]: the directory around
    /// it may be perfectly writable, and a write path puts the file back. The
    /// sentence names the absence rather than a creation failure, because the old
    /// wording sent a person looking for a permissions problem that is not there
    /// (R-6), and the write paths used to fail before they could create it (R-1).
    #[error("the credentials file {path} is not there")]
    Absent {
        /// The file that is not there.
        path: PathBuf,
    },
    /// The file could not be written.
    #[error("{path} cannot be written: {source}")]
    Write {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The mode could not be read or set.
    #[error("the mode of {path} cannot be read or set: {source}")]
    Mode {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file is still reachable by other users after being restricted.
    #[error(
        "{path} has mode {found}, not the {wanted} a credentials file must have; secrets in it are readable by others, so it is refused until the product holds it back"
    )]
    WiderThanAllowed {
        /// The file with the wrong mode.
        path: PathBuf,
        /// The mode found on disk.
        found: String,
        /// The mode that was required.
        wanted: String,
    },
    /// The file is there but could not be read while looking for a key.
    #[error("the credentials file {path} cannot be read: {source}")]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML while looking for a key.
    #[error("the credentials file {path} is not valid TOML: {message}")]
    Syntax {
        /// The file that failed.
        path: PathBuf,
        /// What the parser said, with the line it stopped at.
        message: String,
    },
    /// The file holds no key for that agent.
    #[error(
        "the credentials file {path} records no model key for the agent `{agent}`; add one under [model_keys]"
    )]
    MissingKey {
        /// The file that was read.
        path: PathBuf,
        /// The agent whose key is absent.
        agent: String,
    },
    /// The file holds no Forgejo token under the key that was asked for.
    #[error("the credentials file {path} records no Forgejo value under `{key}`; add one there")]
    MissingToken {
        /// The file that was read.
        path: PathBuf,
        /// The key that was empty or absent.
        key: String,
    },
    /// The file could not be rewritten after an identity was recorded.
    #[error("the credentials file {path} cannot be rewritten: {source}")]
    Rewrite {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
}

impl CredentialsFile {
    /// Create the file with mode 0600 when it is absent, or tighten an existing one,
    /// and read the mode back.
    pub fn prepare(path: &Path) -> Result<Self, CredentialsError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CredentialsError::Parent {
                path: parent.to_path_buf(),
                source,
            })?;
            set_mode(parent, PRIVATE_DIR_MODE).map_err(|source| CredentialsError::Mode {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(CredentialsError::NotAFile {
                        path: path.to_path_buf(),
                    });
                }
                // An existing file may have been made by hand; hold it to 0600.
                set_mode(path, SECRET_FILE_MODE).map_err(|source| CredentialsError::Mode {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(SECRET_FILE_MODE)
                    .open(path)
                    .map_err(|source| CredentialsError::Create {
                        path: path.to_path_buf(),
                        source,
                    })?;
                file.write_all(TEMPLATE.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|source| CredentialsError::Write {
                        path: path.to_path_buf(),
                        source,
                    })?;
                // The process umask can only clear bits, but set the mode outright and
                // then read it back, so the caller never trusts an assumption.
                set_mode(path, SECRET_FILE_MODE).map_err(|source| CredentialsError::Mode {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(CredentialsError::Create {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        let mode = mode_of(path).map_err(|source| CredentialsError::Mode {
            path: path.to_path_buf(),
            source,
        })?;
        if mode != SECRET_FILE_MODE {
            return Err(CredentialsError::WiderThanAllowed {
                path: path.to_path_buf(),
                found: render(mode),
                wanted: render(SECRET_FILE_MODE),
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            mode,
        })
    }

    /// Where the file is. The screen shows this path to the user.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mode the file has on disk.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// The mode, written the way `chmod` writes it.
    pub fn mode_text(&self) -> String {
        render(self.mode)
    }
}

/// A secret, kept where it cannot be printed by accident.
///
/// The value is only reachable through [`Secret::expose`]. Everything else about a
/// `Secret` — formatting it, cloning it, comparing it — is safe.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    value: String,
    /// Other secrets redacted alongside `value`, without being `value` itself.
    ///
    /// A run redacts its own model key and, with it, every other secret the
    /// credentials file holds, so a tool that printed that file cannot carry any of
    /// them into a log or a transcript.
    also: Vec<String>,
}

impl Secret {
    /// A secret with the given value.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            also: Vec::new(),
        }
    }

    /// No secret at all, for a place that has not read one yet.
    pub fn empty() -> Self {
        Self {
            value: String::new(),
            also: Vec::new(),
        }
    }

    /// Whether there is no value.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The value itself.
    ///
    /// This is the one door to the value. It is opened in exactly one place: where
    /// the `Authorization` header of a model request is built. It is never opened for
    /// a log line, a transcript record, a screen or an error message.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Redact these extra secrets alongside the value.
    ///
    /// The value stays what [`Secret::expose`] returns; the extra secrets are only
    /// ever used by [`Secret::redact`]. An empty extra, or one equal to the value, is
    /// ignored.
    pub fn with_extra_redactions(mut self, extras: impl IntoIterator<Item = Secret>) -> Self {
        for extra in extras {
            if !extra.value.is_empty() && extra.value != self.value {
                self.also.push(extra.value);
            }
        }
        self
    }

    /// Replace every occurrence of every held secret in `text` with `<redacted>`.
    ///
    /// This is what every string that leaves the process is passed through, so no
    /// secret the run holds can survive in a transcript, a log or an error even if a
    /// tool printed the credentials file.
    pub fn redact(&self, text: &str) -> String {
        let mut redacted = if self.value.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.value, REDACTED)
        };
        for extra in &self.also {
            redacted = redacted.replace(extra.as_str(), REDACTED);
        }
        redacted
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(")?;
        f.write_str(REDACTED)?;
        f.write_str(")")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Read one agent's model key out of the credentials file.
///
/// The file is held to mode 0600 before it is read, exactly as it is when it is
/// created: a file another user could read is refused rather than used. The key is
/// looked for under `[model_keys]` by the agent's own name, and a single
/// `model_key` at the top level is accepted as a key for every agent. An absent or
/// empty key is a sentence naming the file and the agent, never a guess.
pub fn model_key(path: &Path, agent: &str) -> Result<Secret, CredentialsError> {
    let value = read_value(path)?;
    let key = value
        .get("model_keys")
        .and_then(|keys| keys.get(agent))
        .and_then(toml::Value::as_str)
        .or_else(|| value.get("model_key").and_then(toml::Value::as_str))
        .unwrap_or("");
    if key.trim().is_empty() {
        return Err(CredentialsError::MissingKey {
            path: path.to_path_buf(),
            agent: agent.to_owned(),
        });
    }
    Ok(Secret::new(key.to_owned()))
}

/// Read the token the role check uses, from `forgejo_token`.
pub fn forgejo_token(path: &Path) -> Result<Secret, CredentialsError> {
    let value = read_value(path)?;
    secret_at(path, &value, "forgejo_token")
}

/// Read the administrator's token, from `forgejo_admin_token`, when there is one.
///
/// This is the switch between the two identity paths: an administrator token means
/// the tool creates and manages one identity per agent; no token means the user
/// supplies each agent's own token.
pub fn forgejo_admin_token(path: &Path) -> Result<Option<Secret>, CredentialsError> {
    let value = read_value(path)?;
    match value
        .get("forgejo_admin_token")
        .and_then(toml::Value::as_str)
        .map(str::trim)
    {
        Some(text) if !text.is_empty() => Ok(Some(Secret::new(text.to_owned()))),
        _ => Ok(None),
    }
}

/// Read the Forgejo token that was supplied for one agent.
pub fn forgejo_agent_token(path: &Path, agent: &str) -> Result<Secret, CredentialsError> {
    forgejo_agent_token_optional(path, agent)?.ok_or_else(|| CredentialsError::MissingToken {
        path: path.to_path_buf(),
        key: format!("forgejo_agent_tokens.{agent}"),
    })
}

/// Read the Forgejo token supplied for one agent, when one is there.
pub fn forgejo_agent_token_optional(
    path: &Path,
    agent: &str,
) -> Result<Option<Secret>, CredentialsError> {
    let value = read_value(path)?;
    Ok(value
        .get("forgejo_agent_tokens")
        .and_then(|tokens| tokens.get(agent))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| Secret::new(text.to_owned())))
}

/// Every secret value the credentials file holds, for redacting a run's output.
///
/// A run always redacts its own model key; this returns that key and every other
/// secret beside it — the `forgejo_token`, the `forgejo_admin_token`, the per-agent
/// tokens under `[forgejo_agent_tokens]` and every key under `[model_keys]` — so that
/// a tool which printed the credentials file cannot carry any of them into a log or a
/// transcript. A file that cannot be read yields nothing: the model key the run
/// already holds is still redacted.
pub fn all_secret_values(path: &Path) -> Vec<Secret> {
    let Ok(value) = read_value(path) else {
        return Vec::new();
    };
    let mut secrets: Vec<Secret> = Vec::new();
    for key in ["forgejo_token", "forgejo_admin_token", "model_key"] {
        push_secret(value.get(key).and_then(toml::Value::as_str), &mut secrets);
    }
    for table in ["forgejo_agent_tokens", "model_keys"] {
        if let Some(entries) = value.get(table).and_then(toml::Value::as_table) {
            for text in entries.values().filter_map(toml::Value::as_str) {
                push_secret(Some(text), &mut secrets);
            }
        }
    }
    secrets
}

/// Push one non-empty secret onto a list.
fn push_secret(text: Option<&str>, secrets: &mut Vec<Secret>) {
    if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
        secrets.push(Secret::new(text.to_owned()));
    }
}

/// The *names* of the secrets the file holds, never their values.
///
/// The Credentials screen shows these so a person can confirm a key loaded, while the
/// value itself is never read onto a screen, a log or a command line.
pub fn present_secret_names(path: &Path) -> Vec<String> {
    let Ok(value) = read_value(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if non_empty(value.get("forgejo_token").and_then(toml::Value::as_str)) {
        names.push("Forgejo role token".to_owned());
    }
    if non_empty(value.get("forgejo_admin_token").and_then(toml::Value::as_str)) {
        names.push("Forgejo administrator token".to_owned());
    }
    if non_empty(value.get("model_key").and_then(toml::Value::as_str)) {
        names.push("a model key shared by every agent".to_owned());
    }
    if let Some(keys) = value.get("model_keys").and_then(toml::Value::as_table) {
        for (agent, text) in keys {
            if non_empty(text.as_str()) {
                names.push(format!("model key for `{agent}`"));
            }
        }
    }
    if let Some(tokens) = value.get("forgejo_agent_tokens").and_then(toml::Value::as_table) {
        for (agent, text) in tokens {
            if non_empty(text.as_str()) {
                names.push(format!("Forgejo token for `{agent}`"));
            }
        }
    }
    names.sort();
    names
}

/// Why the credentials file could not be read, when it could not.
///
/// The Credentials screen uses this to tell a file that was read and is empty from one
/// that could not be read at all: a parse failure shown as "nothing yet" is a lie a
/// person acts on.
pub fn parse_problem(path: &Path) -> Option<String> {
    read_value(path).err().map(|error| error.to_string())
}

/// Whether a value is a non-empty string.
fn non_empty(text: Option<&str>) -> bool {
    text.map(str::trim).is_some_and(|text| !text.is_empty())
}

/// The Forgejo login recorded for an agent, when one was recorded.
pub fn recorded_login(path: &Path, agent: &str) -> Result<Option<String>, CredentialsError> {
    let value = read_value(path)?;
    Ok(text_at(&value, "forgejo_logins", agent))
}

/// The email recorded for an agent's identity, when one was recorded.
pub fn recorded_email(path: &Path, agent: &str) -> Result<Option<String>, CredentialsError> {
    let value = read_value(path)?;
    Ok(text_at(&value, "forgejo_emails", agent))
}

/// When the tool created an agent's Forgejo account, in unix seconds, when it did.
///
/// The time is stored so the identity report can say **when** the account was made
/// instead of forever saying `created now` (H-3).
pub fn recorded_identity_created(
    path: &Path,
    agent: &str,
) -> Result<Option<u64>, CredentialsError> {
    let value = read_value(path)?;
    Ok(value
        .get("forgejo_identity_created")
        .and_then(|table| table.get(agent))
        .and_then(toml::Value::as_integer)
        .and_then(|seconds| u64::try_from(seconds).ok()))
}

/// Which identity path the credentials file records, when it records one.
pub fn recorded_identity_mode(path: &Path) -> Result<Option<String>, CredentialsError> {
    let value = read_value(path)?;
    Ok(text_at(&value, "forgejo_identity_mode", ""))
}

/// Record an agent's identity in the credentials file.
///
/// The token is only written when one is given; recording a login and an email for
/// an already-supplied token leaves that token alone. The whole file is rewritten so
/// the change is atomic from a reader's point of view, and it is held to mode 0600.
pub fn record_agent_identity(
    path: &Path,
    agent: &str,
    token: Option<&Secret>,
    login: &str,
    email: &str,
    mode: &str,
    created_at: Option<u64>,
) -> Result<(), CredentialsError> {
    let mut value = value_for_write(path)?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| CredentialsError::MissingToken {
            path: path.to_path_buf(),
            key: "the credentials file".to_owned(),
        })?;
    if let Some(token) = token {
        let tokens = table
            .entry("forgejo_agent_tokens".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        if let Some(tokens) = tokens.as_table_mut() {
            tokens.insert(
                agent.to_owned(),
                toml::Value::String(token.expose().to_owned()),
            );
        }
    }
    let logins = table
        .entry("forgejo_logins".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if let Some(logins) = logins.as_table_mut() {
        logins.insert(agent.to_owned(), toml::Value::String(login.to_owned()));
    }
    let emails = table
        .entry("forgejo_emails".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    if let Some(emails) = emails.as_table_mut() {
        emails.insert(agent.to_owned(), toml::Value::String(email.to_owned()));
    }
    table.insert(
        "forgejo_identity_mode".to_owned(),
        toml::Value::String(mode.to_owned()),
    );
    // The creation time is written when the tool made the account; a later resolve
    // that only reused it leaves the first time in place, so the report ages from when
    // the account was actually created (H-3).
    if let Some(seconds) = created_at {
        let created = table
            .entry("forgejo_identity_created".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        if let Some(created) = created.as_table_mut() {
            created.insert(
                agent.to_owned(),
                toml::Value::Integer(seconds as i64),
            );
        }
    }
    let text = toml::to_string_pretty(&value).map_err(|error| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source: io::Error::other(error),
    })?;
    let text = format!("# gewu-multiverse credentials (the tool last wrote this file)\n{text}");
    crate::mode::write_private(path, text.as_bytes()).map_err(|source| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source,
    })
}

/// Read the file in order to rewrite it, creating it first when it is not there.
///
/// R-1: every write path used to begin with `read_value`, and a missing file was an
/// error before the write could create it — so `k`, `f` and `A` could not put a
/// secret back after the file was deleted in-session, and only a restart recovered.
/// The write paths now put the file back with the file's own custody (the same
/// `CredentialsFile::prepare` that creates it at mode 0600) and then write.
fn value_for_write(path: &Path) -> Result<toml::Value, CredentialsError> {
    match read_value(path) {
        Ok(value) => Ok(value),
        Err(CredentialsError::Absent { .. }) => {
            CredentialsFile::prepare(path)?;
            read_value(path)
        }
        Err(error) => Err(error),
    }
}

/// Whether the credentials file is not there at all (as opposed to unreadable).
///
/// The Credentials screen uses this to say the file is absent and name the key that
/// writes it, instead of reporting a creation failure for a writable directory.
pub fn is_absent(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(source) if source.kind() == io::ErrorKind::NotFound
    )
}

/// Write the secrets a first-run form collected, preserving everything else.
///
/// A value is written but never returned, logged or shown; the screen only ever holds
/// the *name* of a secret.
pub fn record_form_secrets(
    path: &Path,
    agent: &str,
    model_key: Option<&str>,
    forgejo_token: Option<&str>,
) -> Result<(), CredentialsError> {
    let mut value = value_for_write(path)?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| CredentialsError::MissingToken {
            path: path.to_path_buf(),
            key: "the credentials file".to_owned(),
        })?;
    if let Some(token) = forgejo_token.map(str::trim).filter(|text| !text.is_empty()) {
        table.insert(
            "forgejo_token".to_owned(),
            toml::Value::String(token.to_owned()),
        );
    }
    if let Some(key) = model_key.map(str::trim).filter(|text| !text.is_empty()) {
        let keys = table
            .entry("model_keys".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        if let Some(keys) = keys.as_table_mut() {
            keys.insert(agent.to_owned(), toml::Value::String(key.to_owned()));
        }
    }
    let text =
        toml::to_string_pretty(&value).map_err(|error| CredentialsError::Rewrite {
            path: path.to_path_buf(),
            source: io::Error::other(error),
        })?;
    let text = format!("# gewu-multiverse credentials (written by the add-agent form)\n{text}");
    crate::mode::write_private(path, text.as_bytes()).map_err(|source| {
        CredentialsError::Rewrite {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Write one named secret into the credentials file, preserving everything else.
///
/// This is how a secret needed after the agent exists — a rotated model key, the
/// administrator token, or an agent's own Forgejo token — reaches the file without an
/// editor. `table` is the TOML table the key goes under (`model_keys`,
/// `forgejo_agent_tokens`), or `None` for a top-level key (`forgejo_token`,
/// `forgejo_admin_token`). The value is written and never returned.
pub fn record_secret(
    path: &Path,
    table: Option<&str>,
    key: &str,
    value: &str,
) -> Result<(), CredentialsError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let mut root = value_for_write(path)?;
    let root = root
        .as_table_mut()
        .ok_or_else(|| CredentialsError::MissingToken {
            path: path.to_path_buf(),
            key: "the credentials file".to_owned(),
        })?;
    match table {
        Some(name) => {
            let entry = root
                .entry(name.to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let Some(table) = entry.as_table_mut() {
                table.insert(key.to_owned(), toml::Value::String(value.to_owned()));
            }
        }
        None => {
            root.insert(key.to_owned(), toml::Value::String(value.to_owned()));
        }
    }
    let text = toml::to_string_pretty(root).map_err(|error| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source: io::Error::other(error),
    })?;
    let text = format!("# gewu-multiverse credentials (the tool last wrote this file)\n{text}");
    crate::mode::write_private(path, text.as_bytes()).map_err(|source| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source,
    })
}

/// Remove the per-agent secrets for agents that are no longer configured.
///
/// Removing an agent leaves its model key and its own Forgejo token in the credentials
/// file, marked on the Credentials screen as belonging to no configured agent. This
/// deletes exactly those entries, holding the file to its private mode; the values are
/// never read back onto a screen. It returns the labels removed so the screen can say
/// what happened (Y-6).
pub fn remove_agent_secrets(
    path: &Path,
    agents: &[String],
) -> Result<Vec<String>, CredentialsError> {
    // A file that is not there holds nothing to remove; writing one to delete from it
    // would create a file as a side effect of a tidy-up.
    let mut root = match read_value(path) {
        Ok(value) => value,
        Err(CredentialsError::Absent { .. }) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut removed = Vec::new();
    if let Some(root) = root.as_table_mut() {
        for (table, label) in [
            ("model_keys", "model key"),
            ("forgejo_agent_tokens", "Forgejo token"),
        ] {
            if let Some(entries) = root.get_mut(table).and_then(toml::Value::as_table_mut) {
                for agent in agents {
                    if entries.remove(agent).is_some() {
                        removed.push(format!("{label} for `{agent}`"));
                    }
                }
            }
        }
    }
    if removed.is_empty() {
        return Ok(removed);
    }
    let text = toml::to_string_pretty(&root).map_err(|error| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source: io::Error::other(error),
    })?;
    let text = format!("# gewu-multiverse credentials (the tool last wrote this file)\n{text}");
    crate::mode::write_private(path, text.as_bytes()).map_err(|source| CredentialsError::Rewrite {
        path: path.to_path_buf(),
        source,
    })?;
    removed.sort();
    Ok(removed)
}

/// Read the credentials file, refusing one that is not a private plain file.
fn read_value(path: &Path) -> Result<toml::Value, CredentialsError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::Absent {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(CredentialsError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(CredentialsError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    let mode = mode_of(path).map_err(|source| CredentialsError::Mode {
        path: path.to_path_buf(),
        source,
    })?;
    if mode != SECRET_FILE_MODE {
        return Err(CredentialsError::WiderThanAllowed {
            path: path.to_path_buf(),
            found: render(mode),
            wanted: render(SECRET_FILE_MODE),
        });
    }
    let text = fs::read_to_string(path).map_err(|source| CredentialsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|error| CredentialsError::Syntax {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

/// One top-level secret, or a sentence naming the key that is empty or absent.
fn secret_at(path: &Path, value: &toml::Value, key: &str) -> Result<Secret, CredentialsError> {
    match value.get(key).and_then(toml::Value::as_str).map(str::trim) {
        Some(text) if !text.is_empty() => Ok(Secret::new(text.to_owned())),
        _ => Err(CredentialsError::MissingToken {
            path: path.to_path_buf(),
            key: key.to_owned(),
        }),
    }
}

/// The text at one table key, when the file holds a non-empty string there.
///
/// An empty string is the same as absent for every caller: a login, an email or a
/// mode that is empty is not a recorded identity, and reporting it as one put an
/// empty account on a screen (W2-A's sibling on the identity lines).
fn text_at(value: &toml::Value, table: &str, key: &str) -> Option<String> {
    let text = if key.is_empty() {
        value
            .get(table)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    } else {
        value
            .get(table)
            .and_then(|table| table.get(key))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_fresh_file_is_created_with_mode_0600() {
        let root = scratch_root("credentials-mode");
        let path = root.join("credentials.toml");
        let file = CredentialsFile::prepare(&path).expect("the file is prepared");
        assert_eq!(file.mode(), SECRET_FILE_MODE);
        assert_eq!(file.mode_text(), "0600");
        let on_disk = fs::metadata(&path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(on_disk, 0o600);
        let dir_mode = fs::metadata(&root)
            .expect("the directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn the_file_is_not_empty_and_shows_where_a_secret_goes() {
        let root = scratch_root("credentials-template");
        let path = root.join("credentials.toml");
        CredentialsFile::prepare(&path).expect("the file is prepared");
        let text = fs::read_to_string(&path).expect("the template can be read by the test");
        assert!(text.contains("forgejo_token"));
        assert!(
            !text.contains("sk-"),
            "the template must not look like a real secret"
        );
    }

    #[test]
    fn a_wider_mode_is_tightened() {
        let root = scratch_root("credentials-tighten");
        let path = root.join("credentials.toml");
        fs::write(&path, "forgejo_token = \"\"\n").expect("the test writes the file");
        set_mode(&path, 0o644).expect("the test widens the mode");
        assert_eq!(mode_of(&path).expect("the mode is readable"), 0o644);
        let file = CredentialsFile::prepare(&path).expect("the file is tightened");
        assert_eq!(file.mode(), 0o600);
        assert_eq!(mode_of(&path).expect("the mode is readable"), 0o600);
    }

    #[test]
    fn a_file_that_is_a_directory_is_refused() {
        let root = scratch_root("credentials-dir");
        let path = root.join("credentials.toml");
        fs::create_dir(&path).expect("the test makes a directory");
        let error = CredentialsFile::prepare(&path).expect_err("a directory is refused");
        assert!(matches!(error, CredentialsError::NotAFile { .. }));
    }

    #[test]
    fn the_file_is_private_whichever_way_it_was_created() {
        let root = scratch_root("credentials-private");
        let path = root.join("credentials.toml");
        let tight = CredentialsFile::prepare(&path).expect("prepared once");
        let again = CredentialsFile::prepare(&path).expect("prepared twice");
        assert_eq!(tight.mode(), again.mode());
        // Idempotent: preparing a second time does not truncate the file.
        assert!(
            fs::read_to_string(&path)
                .expect("readable")
                .contains("forgejo_token")
        );
    }

    /// Write a credentials file, with a made-up key, held to mode 0600.
    fn credentials_with(tag: &str, text: &str) -> PathBuf {
        let root = scratch_root(tag);
        let path = root.join("credentials.toml");
        crate::mode::write_private(&path, text.as_bytes()).expect("the file is written");
        path
    }

    #[test]
    fn a_secret_cannot_be_printed_by_accident() {
        let secret = Secret::new("sk-a-real-looking-value");
        assert_eq!(format!("{secret}"), REDACTED);
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "sk-a-real-looking-value");
        assert!(!format!("{secret}").contains("sk-"));
    }

    #[test]
    fn redacting_removes_every_occurrence_of_the_value() {
        let secret = Secret::new("sk-abc");
        let text = "key sk-abc and again sk-abc";
        let redacted = secret.redact(text);
        assert_eq!(redacted, "key <redacted> and again <redacted>");
        assert!(!redacted.contains("sk-abc"));
        // An empty secret changes nothing.
        assert_eq!(Secret::empty().redact(text), text);
    }

    #[test]
    fn every_secret_in_the_file_redacts_beside_the_model_key() {
        let path = credentials_with(
            "credentials-all-secrets",
            "forgejo_token = \"forgejo-member-token\"\n\
             forgejo_admin_token = \"forgejo-admin-token\"\n\n\
             [forgejo_agent_tokens]\n\"scribe\" = \"agent-token\"\n\n\
             [model_keys]\n\"scribe\" = \"sk-scribe\"\n\"other\" = \"sk-other\"\n",
        );
        let secret = Secret::new("sk-scribe").with_extra_redactions(all_secret_values(&path));
        // The model key is still the one exposed for the request header.
        assert_eq!(secret.expose(), "sk-scribe");
        let text = "member forgejo-member-token, admin forgejo-admin-token, agent agent-token, \
                    own sk-scribe, other sk-other, plain text";
        let redacted = secret.redact(text);
        for value in [
            "forgejo-member-token",
            "forgejo-admin-token",
            "agent-token",
            "sk-scribe",
            "sk-other",
        ] {
            assert!(
                !redacted.contains(value),
                "{value} survived redaction: {redacted}"
            );
        }
        assert!(redacted.contains("plain text"));
    }

    #[test]
    fn an_empty_or_repeated_extra_secret_is_ignored() {
        let secret = Secret::new("sk-own").with_extra_redactions([
            Secret::empty(),
            Secret::new("sk-own"),
            Secret::new("sk-extra"),
        ]);
        assert_eq!(secret.expose(), "sk-own");
        assert_eq!(secret.redact("sk-own sk-extra"), "<redacted> <redacted>");
    }

    #[test]
    fn a_model_key_is_read_by_the_agents_own_name() {
        let path = credentials_with(
            "credentials-key",
            "forgejo_token = \"\"\n\n[model_keys]\n\"scribe\" = \"sk-scribe-1\"\n\"checker\" = \"sk-checker-2\"\n",
        );
        assert_eq!(
            model_key(&path, "scribe").expect("a key").expose(),
            "sk-scribe-1"
        );
        assert_eq!(
            model_key(&path, "checker").expect("a key").expose(),
            "sk-checker-2"
        );
    }

    #[test]
    fn a_single_top_level_key_is_a_key_for_every_agent() {
        let path = credentials_with("credentials-top-key", "model_key = \"sk-one\"\n");
        assert_eq!(
            model_key(&path, "scribe").expect("a key").expose(),
            "sk-one"
        );
    }

    #[test]
    fn a_missing_key_is_a_sentence_naming_the_file_and_the_agent() {
        let path = credentials_with("credentials-no-key", "[model_keys]\n\"scribe\" = \"\"\n");
        let error = model_key(&path, "scribe").expect_err("the key is absent");
        let message = error.to_string();
        assert!(message.contains("scribe"), "{message}");
        assert!(message.contains("model_keys"), "{message}");
    }

    #[test]
    fn an_agent_the_file_does_not_name_has_no_key() {
        let path = credentials_with(
            "credentials-other-agent",
            "[model_keys]\n\"scribe\" = \"sk-1\"\n",
        );
        assert!(matches!(
            model_key(&path, "checker"),
            Err(CredentialsError::MissingKey { .. })
        ));
    }

    #[test]
    fn a_credentials_file_that_is_too_wide_is_refused_before_its_key_is_read() {
        let path = credentials_with("credentials-wide", "model_key = \"sk-1\"\n");
        set_mode(&path, 0o644).expect("the test widens the mode");
        let error = model_key(&path, "scribe").expect_err("the wide file is refused");
        assert!(matches!(error, CredentialsError::WiderThanAllowed { .. }));
        assert!(!error.to_string().contains("sk-1"), "{error}");
    }

    #[test]
    fn a_missing_credentials_file_is_reported_rather_than_guessed() {
        let root = scratch_root("credentials-absent");
        assert!(model_key(&root.join("nothing.toml"), "scribe").is_err());
    }

    #[test]
    fn broken_toml_in_the_credentials_file_is_reported_as_such() {
        let path = credentials_with("credentials-broken", "[model_keys\n");
        assert!(matches!(
            model_key(&path, "scribe"),
            Err(CredentialsError::Syntax { .. })
        ));
    }

    #[test]
    fn the_forgejo_tokens_are_read_from_their_own_keys() {
        let path = credentials_with(
            "credentials-forgejo",
            "forgejo_token = \"role-token\"\nforgejo_admin_token = \"admin-token\"\n\n[forgejo_agent_tokens]\n\"scribe\" = \"agent-token\"\n",
        );
        assert_eq!(
            forgejo_token(&path).expect("a role token").expose(),
            "role-token"
        );
        assert_eq!(
            forgejo_admin_token(&path)
                .expect("the file reads")
                .expect("an admin token")
                .expose(),
            "admin-token"
        );
        assert_eq!(
            forgejo_agent_token(&path, "scribe")
                .expect("an agent token")
                .expose(),
            "agent-token"
        );
    }

    #[test]
    fn an_absent_or_empty_forgejo_token_is_a_sentence_not_a_guess() {
        let path = credentials_with(
            "credentials-forgejo-empty",
            "forgejo_token = \"\"\nforgejo_admin_token = \"\"\n",
        );
        assert!(matches!(
            forgejo_token(&path),
            Err(CredentialsError::MissingToken { .. })
        ));
        assert!(
            forgejo_admin_token(&path)
                .expect("the file reads")
                .is_none()
        );
        assert!(matches!(
            forgejo_agent_token(&path, "scribe"),
            Err(CredentialsError::MissingToken { .. })
        ));
        assert!(
            forgejo_agent_token_optional(&path, "scribe")
                .expect("the file reads")
                .is_none()
        );
    }

    #[test]
    fn recording_an_identity_writes_the_token_the_login_and_the_mode_privately() {
        let path = credentials_with(
            "credentials-record",
            "forgejo_token = \"role-token\"\nforgejo_admin_token = \"admin-token\"\n\n[model_keys]\n\"scribe\" = \"sk-scribe\"\n",
        );
        record_agent_identity(
            &path,
            "scribe",
            Some(&Secret::new("agent-token")),
            "gewu-scribe",
            "gewu-scribe@agents.invalid",
            "managed",
            Some(1_700_000_000),
        )
        .expect("the identity is recorded");

        // The token is there, and so is everything else the file held.
        assert_eq!(
            forgejo_agent_token(&path, "scribe")
                .expect("the token")
                .expose(),
            "agent-token"
        );
        assert_eq!(
            forgejo_token(&path).expect("the role token").expose(),
            "role-token"
        );
        assert!(
            forgejo_admin_token(&path)
                .expect("the file reads")
                .is_some()
        );
        assert_eq!(
            model_key(&path, "scribe").expect("the model key").expose(),
            "sk-scribe"
        );
        assert_eq!(
            recorded_login(&path, "scribe")
                .expect("the file reads")
                .as_deref(),
            Some("gewu-scribe")
        );
        assert_eq!(
            recorded_email(&path, "scribe")
                .expect("the file reads")
                .as_deref(),
            Some("gewu-scribe@agents.invalid")
        );
        assert_eq!(
            recorded_identity_mode(&path)
                .expect("the file reads")
                .as_deref(),
            Some("managed")
        );
        // The rewrite keeps the file private.
        assert_eq!(
            mode_of(&path).expect("the mode is readable"),
            SECRET_FILE_MODE
        );
        // H-3: the creation time is written down, and a later resolve that reuses the
        // account leaves the first time in place instead of overwriting it.
        assert_eq!(
            recorded_identity_created(&path, "scribe").expect("the file reads"),
            Some(1_700_000_000)
        );
        record_agent_identity(
            &path,
            "scribe",
            None,
            "gewu-scribe",
            "gewu-scribe@agents.invalid",
            "managed",
            None,
        )
        .expect("the reuse is recorded");
        assert_eq!(
            recorded_identity_created(&path, "scribe").expect("the file reads"),
            Some(1_700_000_000),
            "a reuse must not lose the account's creation time"
        );
    }

    #[test]
    fn recording_an_identity_without_a_token_leaves_the_supplied_token_alone() {
        let path = credentials_with(
            "credentials-record-supplied",
            "[forgejo_agent_tokens]\n\"scribe\" = \"supplied-token\"\n",
        );
        record_agent_identity(
            &path,
            "scribe",
            None,
            "author",
            "author@example.org",
            "supplied",
            None,
        )
        .expect("the identity is recorded");
        assert_eq!(
            forgejo_agent_token(&path, "scribe")
                .expect("the token")
                .expose(),
            "supplied-token"
        );
        assert_eq!(
            recorded_login(&path, "scribe")
                .expect("the file reads")
                .as_deref(),
            Some("author")
        );
        assert_eq!(
            recorded_identity_mode(&path)
                .expect("the file reads")
                .as_deref(),
            Some("supplied")
        );
    }

    // R-1 (severity 3): a credentials file deleted while the UI runs could not be put
    // back in-session, because every write path began with a read that mapped
    // `NotFound` to an error before the create-and-write could run. The guard deletes
    // the file and writes again, exactly as `k`/`f`/`A` do after a person deletes it.
    #[test]
    fn a_deleted_credentials_file_is_put_back_by_a_secret_write() {
        let root = scratch_root("credentials-deleted-secret");
        let path = root.join("credentials.toml");
        CredentialsFile::prepare(&path).expect("the file is prepared");
        record_secret(&path, Some("model_keys"), "scribe", "sk-restored")
            .expect("the first write works");
        fs::remove_file(&path).expect("the test deletes the file");
        assert!(is_absent(&path), "the file is gone");
        record_secret(&path, Some("model_keys"), "scribe", "sk-restored")
            .expect("a write puts the file back");
        assert!(!is_absent(&path), "the file is back");
        assert_eq!(
            mode_of(&path).expect("the mode is readable"),
            SECRET_FILE_MODE,
            "the file that was put back keeps the private mode"
        );
        assert_eq!(
            model_key(&path, "scribe").expect("the key").expose(),
            "sk-restored"
        );
    }

    #[test]
    fn a_deleted_credentials_file_is_put_back_by_the_form_and_by_an_identity() {
        let root = scratch_root("credentials-deleted-form");
        let path = root.join("credentials.toml");
        CredentialsFile::prepare(&path).expect("the file is prepared");
        fs::remove_file(&path).expect("the test deletes the file");
        record_form_secrets(&path, "scribe", Some("sk-form"), Some("role-token"))
            .expect("the form puts the file back");
        assert_eq!(
            model_key(&path, "scribe").expect("the key").expose(),
            "sk-form"
        );
        assert_eq!(
            forgejo_token(&path).expect("the token").expose(),
            "role-token"
        );
        fs::remove_file(&path).expect("the test deletes the file again");
        record_agent_identity(
            &path,
            "scribe",
            Some(&Secret::new("agent-token")),
            "gewu-scribe",
            "gewu-scribe@agents.invalid",
            "managed",
            None,
        )
        .expect("settling an identity puts the file back");
        assert_eq!(
            forgejo_agent_token(&path, "scribe")
                .expect("the agent token")
                .expose(),
            "agent-token"
        );
        assert_eq!(mode_of(&path).expect("the mode"), SECRET_FILE_MODE);
    }

    #[test]
    fn a_missing_file_is_reported_as_absent_not_as_a_creation_failure() {
        let root = scratch_root("credentials-absent-words");
        let path = root.join("credentials.toml");
        let error = model_key(&path, "scribe").expect_err("there is no file");
        assert!(
            matches!(error, CredentialsError::Absent { .. }),
            "a missing file is not a creation failure: {error}"
        );
        let message = error.to_string();
        assert!(message.contains("is not there"), "{message}");
        assert!(
            !message.contains("cannot be created") && !message.contains("os error"),
            "the wrong cause is named: {message}"
        );
        assert!(is_absent(&path));
    }

    #[test]
    fn removing_leftovers_from_an_absent_file_creates_nothing() {
        let root = scratch_root("credentials-absent-remove");
        let path = root.join("credentials.toml");
        let removed = remove_agent_secrets(&path, &["ghost".to_owned()])
            .expect("an absent file has nothing to remove");
        assert!(removed.is_empty());
        assert!(is_absent(&path), "a tidy-up must not create a file");
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;
    use crate::test_support::scratch_root;

    #[test]
    fn the_form_writes_a_key_and_a_token_and_preserves_the_rest() {
        let root = scratch_root("credentials-form");
        let path = root.join("credentials.toml");
        CredentialsFile::prepare(&path).expect("the file is prepared");
        record_form_secrets(&path, "scribe", Some("sk-form-key"), Some("role-token"))
            .expect("the form's secrets are written");
        // The token and the key are there.
        assert_eq!(
            forgejo_token(&path).expect("a token").expose(),
            "role-token"
        );
        assert_eq!(
            model_key(&path, "scribe").expect("a key").expose(),
            "sk-form-key"
        );
        // A second call preserves what the first wrote.
        record_form_secrets(&path, "other", Some("sk-other"), None)
            .expect("a second agent is written");
        assert_eq!(
            forgejo_token(&path).expect("a token").expose(),
            "role-token"
        );
        assert_eq!(
            model_key(&path, "scribe").expect("a key").expose(),
            "sk-form-key"
        );
    }

    #[test]
    fn the_present_names_are_names_and_never_values() {
        let root = scratch_root("credentials-present");
        let path = root.join("credentials.toml");
        CredentialsFile::prepare(&path).expect("the file is prepared");
        record_form_secrets(&path, "scribe", Some("sk-form-key"), Some("role-token"))
            .expect("the form's secrets are written");
        let names = present_secret_names(&path);
        assert!(names.iter().any(|name| name == "model key for `scribe`"));
        assert!(names.iter().any(|name| name == "Forgejo role token"));
        for name in &names {
            assert!(!name.contains("sk-form-key"), "{name}");
            assert!(!name.contains("role-token"), "{name}");
        }
    }
}
