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
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(SECRET_FILE_MODE)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_mode(path, SECRET_FILE_MODE)
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
