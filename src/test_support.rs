//! Helpers for the unit tests in this crate.
//!
//! A scratch root is a directory under the system temporary directory, named after
//! the test that asked for it, the process, and the current nanosecond. No test ever
//! touches the user's real configuration directory.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh, empty directory for one test.
pub(crate) fn scratch_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!(
        "gewu-multiverse-test-{tag}-{}-{nanos}",
        process::id()
    ));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A fresh, empty directory with a short path.
///
/// Some product sentences are bounded (600 bytes) and repeat a path; a root under
/// a long `TMPDIR` can push the sentence they belong to past its bound. A test
/// that reads such a sentence whole asks for this root instead of assuming that
/// the system temporary directory is short.
pub(crate) fn short_scratch_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = Path::new("/tmp").join(format!("gewu-{tag}-{}-{nanos}", process::id()));
    if fs::create_dir_all(&root).is_ok() {
        return root;
    }
    scratch_root(tag)
}
