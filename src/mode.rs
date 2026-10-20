//! The file modes the two files on disk are held to.
//!
//! The credentials file holds secrets, so it is created as 0600 and checked back:
//! the program never assumes the mode took. The directory around it is 0700, so the
//! files are not reachable through a directory others can walk.

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The only mode the credentials file may have: its user alone may read and write it.
pub const SECRET_FILE_MODE: u32 = 0o600;

/// The mode of the directory holding the files: its user alone may enter it.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Set the mode of an existing path (a file or a directory).
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// The mode of an existing path, with the file-type bits removed.
pub fn mode_of(path: &Path) -> io::Result<u32> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o7777)
}

/// Hold one existing directory, and every directory between it and `stop`, to mode
/// 0700. `stop` itself is included; nothing above it is ever touched, and a path
/// that is not below `stop` is held to 0700 on its own only.
///
/// `fs::create_dir_all` makes the intermediate directories with the process umask,
/// so a path like `<root>/agents/<name>` would otherwise leave `agents/` at `0775`
/// while `<name>` is `0700` (F-1). This walks the chain the product owns and gives
/// every link the one custody the product promises for its directories.
pub fn hold_dir_private_below(stop: &Path, path: &Path) -> io::Result<()> {
    if !path.starts_with(stop) {
        return set_mode(path, PRIVATE_DIR_MODE);
    }
    let mut current = Some(path);
    while let Some(dir) = current {
        set_mode(dir, PRIVATE_DIR_MODE)?;
        if dir == stop {
            break;
        }
        current = dir.parent();
    }
    Ok(())
}

/// Hold every path under one of the product's own roots to the one custody.
///
/// The product holds each directory it *creates* to `0700` and each file it *writes*
/// to `0600`, but a tree an older version left behind is never revisited, so an old
/// run directory or configuration file keeps the umask mode it was made with (R-1).
/// This walks a root and holds its directories to `0700` and its plain files to
/// `0600`.
///
/// A directory named `workspace` is held itself but **not** descended into: the clone
/// inside it is git's, not the product's, and its modes are not the product's to
/// change (the workspace holds no secret the custody rule protects; the `0700`
/// workspace is what blocks traversal). Symlinks are never followed, and a path that
/// cannot be changed is left as it is rather than failing the load.
pub fn hold_tree_private(root: &Path) {
    if !root.is_dir() {
        return;
    }
    let _ = set_mode(root, PRIVATE_DIR_MODE);
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let _ = set_mode(&path, PRIVATE_DIR_MODE);
            if path
                .file_name()
                .map(|name| name == "workspace")
                .unwrap_or(false)
            {
                continue;
            }
            hold_tree_private(&path);
        } else if file_type.is_file() {
            let _ = set_mode(&path, SECRET_FILE_MODE);
        }
    }
}

/// Render a mode the way `chmod` writes it, for people to read.
pub fn render(mode: u32) -> String {
    format!("{:04o}", mode)
}

/// Write a whole file with mode 0600, replacing whatever was there.
///
/// The mode is set at creation and checked afterwards, so a file that already
/// existed with a wider mode does not keep it.
pub fn write_private(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    // A configuration file may be a symlink the person made on purpose. Write the
    // file it points at, so the link survives the atomic rename.
    let path = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    // A rename into a writable directory can replace a read-only file, which the old
    // truncate-and-write could not do. Keep the refusal a read-only target earned:
    // the caller's one sentence says the file cannot be written.
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is read-only", path.display()),
            ));
        }
    }
    // Write a temporary file beside the target and rename it into place, so another
    // process reading a secret while it is rewritten sees either the old file or the
    // new one, never a half-written line.
    let stage = path.with_extension(format!("part-{}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(SECRET_FILE_MODE)
            .open(&stage)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        set_mode(&stage, SECRET_FILE_MODE)?;
        fs::rename(&stage, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_rendered_with_leading_zeroes() {
        assert_eq!(render(0o600), "0600");
        assert_eq!(render(0o700), "0700");
    }

    #[test]
    fn a_tree_from_an_older_version_is_held_back_to_the_one_custody() {
        // R-1: a tree an older version wrote keeps its umask modes; the product holds
        // its own roots to the custody it promises on every load. A workspace's
        // contents are git's and are not touched.
        let root = crate::test_support::scratch_root("mode-hold-tree");
        let run = root.join("state").join("scribe").join("run-1");
        let workspace = root.join("agents").join("scribe").join("workspace");
        let clone = workspace.join("notes");
        fs::create_dir_all(&run).expect("the run directory");
        fs::create_dir_all(&clone).expect("the clone directory");
        let config = root.join("multiverse.toml");
        fs::write(&config, "x\n").expect("a file");
        let transcript = run.join("transcript.jsonl");
        fs::write(&transcript, "{}\n").expect("a file");
        let git_file = clone.join("HEAD");
        fs::write(&git_file, "ref\n").expect("a git file");
        for path in [&run, &workspace, &clone] {
            set_mode(path, 0o775).expect("widened for the test");
        }
        set_mode(&config, 0o664).expect("widened for the test");
        set_mode(&transcript, 0o664).expect("widened for the test");
        set_mode(&git_file, 0o644).expect("git's own mode");

        hold_tree_private(&root);

        for dir in [&run, &workspace] {
            assert_eq!(mode_of(dir).expect("mode"), PRIVATE_DIR_MODE, "{dir:?}");
        }
        assert_eq!(mode_of(&config).expect("mode"), SECRET_FILE_MODE);
        assert_eq!(mode_of(&transcript).expect("mode"), SECRET_FILE_MODE);
        assert_eq!(
            mode_of(&clone).expect("mode"),
            0o775,
            "a clone below the workspace is git's, not the product's"
        );
        assert_eq!(
            mode_of(&git_file).expect("mode"),
            0o644,
            "the clone's contents are git's, not the product's"
        );
    }

    #[test]
    fn a_private_write_always_leaves_mode_0600() {
        let root = crate::test_support::scratch_root("mode-private");
        let path = root.join("a-file");
        write_private(&path, b"first").expect("written");
        assert_eq!(mode_of(&path).expect("mode readable"), 0o600);
        set_mode(&path, 0o644).expect("widened for the test");
        write_private(&path, b"second").expect("written again");
        assert_eq!(mode_of(&path).expect("mode readable"), 0o600);
        assert_eq!(fs::read_to_string(&path).expect("readable"), "second");
    }
}
