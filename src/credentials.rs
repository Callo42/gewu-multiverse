//! The one credentials file.
//!
//! This file is the only place a secret lives. The skeleton does three things with
//! it and nothing else:
//!
//! 1. create it, with mode 0600, when it is not there yet;
//! 2. tighten its mode to 0600 if it is wider than that;
//! 3. read back the mode, so the mode shown on the Credentials screen is the mode on
//!    disk rather than an assumption.
//!
//! It is never read into memory, never printed, never written to a log, and never
//! passed on a command line. That is why there is no function here that returns the
//! contents of the file: there is nothing to leak by accident, because nothing
//! loads it.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::mode::{PRIVATE_DIR_MODE, SECRET_FILE_MODE, mode_of, render, set_mode};

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
}
