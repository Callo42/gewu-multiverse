//! The file modes the two files on disk are held to.
//!
//! The credentials file holds secrets, so it is created as 0600 and checked back:
//! the program never assumes the mode took. The directory around it is 0700, so the
//! files are not reachable through a directory others can walk.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
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

/// Render a mode the way `chmod` writes it, for people to read.
pub fn render(mode: u32) -> String {
    format!("{:04o}", mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_rendered_with_leading_zeroes() {
        assert_eq!(render(0o600), "0600");
        assert_eq!(render(0o700), "0700");
    }
}
