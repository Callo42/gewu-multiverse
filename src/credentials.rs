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
//! behind.

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
# Put the token you use to reach your Forgejo here.

forgejo_token = ""

[model_keys]
# One key per agent name. For an agent called example-agent:
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
        "{path} has mode {found} and could not be held to mode {wanted}; secrets in it are readable by others, so it is refused"
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
pub struct Secret(String);

impl Secret {
    /// A secret with the given value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// No secret at all, for a place that has not read one yet.
    pub fn empty() -> Self {
        Self(String::new())
    }

    /// Whether there is no value.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value itself.
    ///
    /// This is the one door to the value. It is opened in exactly one place: where
    /// the `Authorization` header of a model request is built. It is never opened for
    /// a log line, a transcript record, a screen or an error message.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Replace every occurrence of the value in `text` with `<redacted>`.
    ///
    /// This is what every string that leaves the process is passed through, so the
    /// key cannot survive in a transcript, a log or an error even if a tool printed
    /// the credentials file.
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.0, REDACTED)
        }
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
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(CredentialsError::Create {
                path: path.to_path_buf(),
                source,
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
    let value: toml::Value = toml::from_str(&text).map_err(|error| CredentialsError::Syntax {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
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
}
