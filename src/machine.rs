//! Which machine a fact was learned on.
//!
//! The configuration directory, the credentials file, the acknowledgment record, the
//! endpoint-test record and the run history all live on one machine, and a person can
//! copy the configuration directory to another. A fact that was learned by running
//! something — an endpoint test, an acknowledgment, a run — belongs to the machine
//! that produced it. This module names the machine, so a record can carry the name and
//! a screen can tell a fact made here from one made elsewhere instead of presenting
//! the second machine's history as its own (W32-2, W32-6).
//!
//! The mark is the host name and, when it can be read, the kernel's stable
//! `/etc/machine-id`. Both survive a reboot in the ordinary case, so a test made
//! before a restart is still a test made on this machine; a host name alone is enough
//! to tell two hosts apart. When neither can be read the mark is [`None`], and a
//! surface says the record does not name its machine rather than claiming anything.

use std::fs;

/// Read one small text file, trimmed, when it is there and not empty.
fn read_mark(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The mark this machine's facts carry, when it can be read.
///
/// `host/machine-id` when both are readable, the one that is readable otherwise, and
/// `None` when neither is. The value names a machine; it is not a secret and holds
/// nothing the user typed.
pub fn mark() -> Option<String> {
    let host = read_mark("/proc/sys/kernel/hostname");
    let id = read_mark("/etc/machine-id").or_else(|| read_mark("/var/lib/dbus/machine-id"));
    match (host, id) {
        (Some(host), Some(id)) => Some(format!("{host}/{id}")),
        (Some(host), None) => Some(host),
        (None, Some(id)) => Some(id),
        (None, None) => None,
    }
}

/// How a record's machine relates to the machine reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The record names this machine.
    Here,
    /// The record names a different machine.
    Elsewhere,
    /// The record does not say which machine it was made on.
    Unnamed,
    /// The record names a machine, but this machine's own mark cannot be read, so the
    /// two cannot be compared.
    Undecidable,
}

/// Compare a record's machine mark with this machine.
pub fn relation(record: &str) -> Relation {
    let record = record.trim();
    if record.is_empty() {
        return Relation::Unnamed;
    }
    match mark() {
        Some(here) if here == record => Relation::Here,
        Some(_) => Relation::Elsewhere,
        None => Relation::Undecidable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_is_readable_on_this_host() {
        // A machine whose host name and machine-id can both be read produces a mark;
        // a container without them produces `None`. Neither is an error.
        let mark = mark();
        if let Some(mark) = &mark {
            assert!(!mark.is_empty());
        }
    }

    #[test]
    fn a_record_with_no_machine_is_unnamed() {
        assert_eq!(relation(""), Relation::Unnamed);
        assert_eq!(relation("   "), Relation::Unnamed);
    }

    #[test]
    fn the_machines_own_mark_is_here_and_another_is_elsewhere() {
        let Some(here) = mark() else {
            return;
        };
        assert_eq!(relation(&here), Relation::Here);
        assert_eq!(relation("another-host/not-this-one"), Relation::Elsewhere);
        assert_ne!(relation(&here), Relation::Elsewhere);
    }
}
