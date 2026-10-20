//! The version a configuration root was last opened by.
//!
//! `last-seen.toml` sits beside `multiverse.toml`. It holds one fact only: the newest
//! version that has opened this configuration. It is written when the engine opens
//! and again on a clean shutdown, so the next build can tell an upgrade from a fresh
//! installation and show the one harmless screen that says what was found.

use std::fs;
use std::path::Path;

/// The file's name in the configuration root.
pub const FILE: &str = "last-seen.toml";

/// The version recorded by the last build that opened this root, when there is one.
pub fn read(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join(FILE)).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("version")
        .and_then(toml::Value::as_str)
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
}

/// Record this build's version. A clean open and a clean exit both call this.
pub fn write(root: &Path, version: &str) -> Result<(), String> {
    let text = format!(
        "# The newest version that has opened this configuration.\nversion = \"{}\"\n",
        version.trim()
    );
    crate::mode::write_private(root.join(FILE), text.as_bytes())
        .map_err(|error| format!("{} cannot be written: {error}", root.join(FILE).display()))
}

/// Whether `older` names an earlier version than `newer`.
///
/// The comparison is numeric part by part; a version with fewer parts is padded with
/// zeros. A non-numeric part stops the comparison, which is the safe direction: an
/// unrecognised version is not treated as an upgrade.
pub fn is_older(older: &str, newer: &str) -> bool {
    fn parts(version: &str) -> Option<Vec<u64>> {
        version
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    }
    let (Some(mut older), Some(mut newer)) = (parts(older), parts(newer)) else {
        return false;
    };
    let width = older.len().max(newer.len());
    older.resize(width, 0);
    newer.resize(width, 0);
    older < newer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_compared_numerically() {
        assert!(is_older("0.1.29", "0.1.30"));
        assert!(is_older("0.9.9", "0.10.0"));
        assert!(is_older("v0.1.29", "0.1.30"));
        assert!(!is_older("0.1.30", "0.1.30"));
        assert!(!is_older("0.1.31", "0.1.30"));
        assert!(!is_older("not a version", "0.1.30"));
    }

    #[test]
    fn a_written_version_reads_back() {
        let root = crate::test_support::scratch_root("last-seen");
        assert_eq!(read(&root), None);
        write(&root, "0.1.29").expect("the version is written");
        assert_eq!(read(&root).as_deref(), Some("0.1.29"));
        let text = fs::read_to_string(root.join(FILE)).expect("the file");
        assert!(text.contains("# The newest version"));
    }
}
