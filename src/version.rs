//! The version a configuration has been used by, and how two versions compare.
//!
//! U3: after an upgrade, running an **older** build against a `HOME` a newer release
//! has used said nothing. The acknowledgment line compared the version the record was
//! made under with the running build, but only in the forward direction: a build older
//! than the data had no word for "this configuration has since been used by a newer
//! version". The `last-seen` record is written by every start with the newest version
//! that has opened the configuration, and the screens compare the two with the one
//! [`compare`] below.
//!
//! The record holds no secret: a version string and the instant the newest version
//! opened the configuration. It is `0600` like every other file the product writes.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::mode::write_private;
use crate::time::now_unix_ms;

/// The version this build is, from `Cargo.toml`.
pub fn running() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Compare two dotted version strings, numerically.
///
/// A component that is not a number counts as `0`, and a missing component is `0`, so
/// `0.1.9 < 0.1.10` (the reason a string comparison is not enough) and `0.2` is newer
/// than `0.1.99`. The comparison is total: it never panics and never guesses an
/// ordering it cannot justify.
pub fn compare(left: &str, right: &str) -> Ordering {
    let left: Vec<u64> = left.trim().split('.').map(component).collect();
    let right: Vec<u64> = right.trim().split('.').map(component).collect();
    let count = left.len().max(right.len());
    for at in 0..count {
        let a = left.get(at).copied().unwrap_or(0);
        let b = right.get(at).copied().unwrap_or(0);
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

/// One dotted component, read as a number; anything else is `0`.
fn component(text: &str) -> u64 {
    text.trim()
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse().ok())
        .unwrap_or(0)
}

/// Whether `left` is strictly newer than `right`.
pub fn is_newer(left: &str, right: &str) -> bool {
    compare(left, right) == Ordering::Greater
}

/// The newest version that has opened a configuration, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSeen {
    /// The version string the record carries.
    pub version: String,
    /// Seconds since the Unix epoch, when that version opened the configuration.
    pub opened_at_unix: u64,
}

/// Read the `last-seen` record at `path`, or `None` when it is missing or unreadable.
///
/// This record is a courtesy, not a gate: a missing or unparsable one says only that
/// the product cannot tell which version opened the configuration last, and no start
/// is refused because of it.
pub fn read(path: &Path) -> Option<LastSeen> {
    let text = fs::read_to_string(path).ok()?;
    let record: Record = toml::from_str(&text).ok()?;
    let version = record.version.trim().to_owned();
    if version.is_empty() {
        return None;
    }
    Some(LastSeen {
        version,
        opened_at_unix: record.opened_at_unix,
    })
}

/// Record that this build has opened the configuration, keeping the newest version.
///
/// The record is written when this build is at least as new as the one already
/// recorded. An **older** build leaves it alone, so the newest version that has
/// touched the configuration is not forgotten the moment someone goes back — which is
/// exactly the fact U3 needs.
pub fn note_started(path: &Path, version: &str) -> io::Result<LastSeen> {
    let previous = read(path);
    if let Some(seen) = &previous {
        if is_newer(&seen.version, version) {
            return Ok(seen.clone());
        }
    }
    let opened_at_unix = now_unix_ms() / 1000;
    let text = format!(
        "# Written when this configuration is opened. It holds no secret: the newest\n\
         # version that has opened the configuration, and when. An older build leaves\n\
         # it as it is, so going back to an older release is not silent (U3).\n\
         version = {}\n\
         opened_at_unix = {opened_at_unix}\n",
        toml::Value::String(version.to_owned())
    );
    write_private(path, text.as_bytes())?;
    Ok(LastSeen {
        version: version.to_owned(),
        opened_at_unix,
    })
}

/// The shape of the `last-seen` record on disk.
#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    version: String,
    #[serde(default)]
    opened_at_unix: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;

    #[test]
    fn dotted_versions_compare_numerically_not_as_text() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.2", "0.1.99"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.9", "0.1.10"));
        assert_eq!(compare("0.1.21", "0.1.21"), Ordering::Equal);
        // A component that is not a number is zero, and a missing one is zero.
        assert_eq!(compare("0.1", "0.1.0"), Ordering::Equal);
        assert!(is_newer("0.1.1", "0.1"));
    }

    #[test]
    fn a_newer_build_updates_the_record_and_an_older_one_leaves_it() {
        let root = scratch_root("version-last-seen");
        let path = root.join("last-seen.toml");
        // The first start records the running build.
        let first = note_started(&path, "0.1.21").expect("the record is written");
        assert_eq!(first.version, "0.1.21");
        // A newer build moves it forward.
        let forward = note_started(&path, "0.1.22").expect("the record is updated");
        assert_eq!(forward.version, "0.1.22");
        // An older build does not move it back: the fact U3 needs survives the
        // reverse direction.
        let back = note_started(&path, "0.1.20").expect("the record is left");
        assert_eq!(back.version, "0.1.22");
        assert_eq!(read(&path).expect("still there").version, "0.1.22");
    }

    #[test]
    fn the_record_is_private_and_a_missing_one_is_not_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("version-mode");
        let path = root.join("last-seen.toml");
        assert!(read(&path).is_none());
        note_started(&path, "0.1.21").expect("the record is written");
        let mode = fs::metadata(&path)
            .expect("the record exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the record is not private");
    }
}
