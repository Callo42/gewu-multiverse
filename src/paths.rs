//! Where the three files live.
//!
//! One root directory holds everything: `config.toml`, `credentials.toml` and the
//! first-run acknowledgment. On a Linux machine the root is
//! `~/.config/gewu-multiverse`, or `$XDG_CONFIG_HOME/gewu-multiverse` when that
//! variable is set. `--config-dir` moves the root for one run, which is what the
//! tests and the screen evidence use.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::mode::{PRIVATE_DIR_MODE, set_mode};

/// Every path this program uses, derived from one root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

/// Why the root directory could not be decided or prepared.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// No home directory and no `XDG_CONFIG_HOME`: there is nowhere to put the files.
    #[error(
        "no configuration directory could be found: set HOME or XDG_CONFIG_HOME, or pass --config-dir"
    )]
    NoConfigDir,
    /// The root directory could not be created.
    #[error("the configuration directory {path} cannot be created: {source}")]
    Root {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The root directory could not be held to mode 0700.
    #[error("the configuration directory {path} cannot be restricted to mode 0700: {source}")]
    RootMode {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
}

impl Layout {
    /// The root under the user's own configuration directory.
    pub fn in_user_config_dir() -> Result<Self, PathError> {
        let base = BaseDirs::new().ok_or(PathError::NoConfigDir)?;
        Ok(Self::rooted_at(base.config_dir().join("gewu-multiverse")))
    }

    /// A root the caller chose, which is what `--config-dir` and the tests pass.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory holding the three files.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The one configuration file.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The one credentials file. Its contents are never read by this skeleton.
    pub fn credentials_file(&self) -> PathBuf {
        self.root.join("credentials.toml")
    }

    /// The record of the first-run risk acknowledgment.
    pub fn acknowledgment_file(&self) -> PathBuf {
        self.root.join("risk-acknowledgment.toml")
    }

    /// Create the root directory if it is not there yet, and hold it to mode 0700.
    pub fn ensure_root(&self) -> Result<(), PathError> {
        fs::create_dir_all(&self.root).map_err(|source| PathError::Root {
            path: self.root.clone(),
            source,
        })?;
        set_mode(&self.root, PRIVATE_DIR_MODE).map_err(|source| PathError::RootMode {
            path: self.root.clone(),
            source,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;

    #[test]
    fn the_three_files_sit_in_the_root() {
        let layout = Layout::rooted_at("/tmp/example-root");
        assert_eq!(
            layout.config_file(),
            Path::new("/tmp/example-root/config.toml")
        );
        assert_eq!(
            layout.credentials_file(),
            Path::new("/tmp/example-root/credentials.toml")
        );
        assert_eq!(
            layout.acknowledgment_file(),
            Path::new("/tmp/example-root/risk-acknowledgment.toml")
        );
    }

    #[test]
    fn ensure_root_creates_the_directory_once_and_keeps_it_private() {
        let root = scratch_root("layout");
        let layout = Layout::rooted_at(root.join("nested"));
        layout.ensure_root().expect("the root is created");
        layout.ensure_root().expect("a second call is harmless");
        assert!(layout.root().is_dir());
        let mode = fs::metadata(layout.root())
            .expect("the root can be described")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_DIR_MODE);
    }

    use std::os::unix::fs::PermissionsExt;
}
