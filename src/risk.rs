//! The first-run risk acknowledgment.
//!
//! Before any agent may be started, the user is shown, once, what running an agent
//! means on this machine, and has to confirm it. The confirmation is written to one
//! small file next to the configuration file, so a later start is not gated again —
//! and so a fresh installation (an empty configuration directory) shows it again.
//!
//! Reading the record fails closed: a missing, unreadable or unparsable record is
//! treated as "not confirmed", never as "confirmed".

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::mode::{SECRET_FILE_MODE, set_mode};

/// The heading of the acknowledgment screen.
pub const TITLE: &str = "Before the first agent runs";

/// What the user is asked to understand, in plain language. Each line is short
/// enough to sit on one row of the acknowledgment screen at its usual width.
pub const LINES: &[&str] = &[
    "An agent is a program this tool runs for you.",
    "You give it an instruction and point it at a model.",
    "An agent runs as you: the same user, this machine, your rights.",
    "Its workspace is a directory. That is a layout, not a sandbox.",
    "An agent can read anything you can read — the credentials file too.",
    "The Credentials screen shows you where that file is.",
    "On your Forgejo, an agent acts with the identity and token you give it.",
    "Nothing starts an agent until you confirm this screen once.",
];

/// The state of the record on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckState {
    /// No record yet: a fresh installation, or the record was removed.
    Missing,
    /// The risk was confirmed and the time it was confirmed is known.
    Recorded {
        /// Seconds since the Unix epoch, as written when it was confirmed.
        acknowledged_at_unix: u64,
        /// The version that wrote the record, as the file itself carries it.
        ///
        /// A second person on the same configuration directory inherits terms that
        /// were accepted under this version (C-3); the value is read from the file,
        /// never assumed to be the running version.
        app_version: String,
        /// The machine the confirmation was made on, when the record names one.
        ///
        /// The modal's own sentence says `this machine`, and a configuration
        /// directory copied to a second machine carries the record with it; the mark
        /// lets the screen say whose acknowledgment this is (W32-6).
        machine: Option<String>,
    },
    /// A record is there but cannot be trusted, so it is not treated as a confirmation.
    Unreadable {
        /// What went wrong, for the screen to show.
        reason: String,
    },
}

impl AckState {
    /// Read the record at `path`. Never fails: a problem is a state of its own.
    pub fn read(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return AckState::Missing,
            Err(source) => {
                return AckState::Unreadable {
                    reason: source.to_string(),
                };
            }
        };
        match toml::from_str::<Record>(&text) {
            Ok(record) if record.acknowledged => AckState::Recorded {
                acknowledged_at_unix: record.acknowledged_at_unix,
                app_version: record.app_version,
                machine: record.machine,
            },
            Ok(_) => AckState::Missing,
            Err(error) => AckState::Unreadable {
                reason: error.to_string(),
            },
        }
    }

    /// Whether an agent may be started. Only a record that was read and understood
    /// counts; everything else refuses.
    pub fn is_confirmed(&self) -> bool {
        matches!(self, AckState::Recorded { .. })
    }

    /// One sentence for the screen.
    pub fn describe(&self) -> String {
        match self {
            AckState::Missing => {
                "not acknowledged yet — this installation has no record of a confirmation"
                    .to_owned()
            }
            AckState::Recorded {
                acknowledged_at_unix,
                app_version,
                ..
            } => format!(
                "acknowledged on {} (version {})",
                crate::time::compact_utc(acknowledged_at_unix.saturating_mul(1000)),
                version_label(app_version)
            ),
            AckState::Unreadable { reason } => format!(
                "a record is there but cannot be read ({reason}); it is not treated as a confirmation"
            ),
        }
    }

    /// The line the Agents note area draws when a record exists.
    ///
    /// A first user answers the overlay once and never thinks about it again; a
    /// **second** person on the same configuration directory is running agents under
    /// a risk someone else accepted, under a version they did not run. The record is
    /// on disk — this line puts it on the screen, with the key that shows the terms
    /// again, so reading them does not mean finding and deleting a file by hand
    /// (C-3). Both versions are named and labelled: the one the record agreed to and
    /// the one now running, so the comparison a Migrator came for can be made on the
    /// screen (F3).
    pub fn record_line(&self) -> Option<String> {
        match self {
            AckState::Recorded {
                acknowledged_at_unix,
                app_version,
                machine,
            } => Some(format!(
                "the risk was acknowledged on {} (version {}; this build is {}; {}) — press R to read the terms again; the record is not changed",
                crate::time::compact_utc(acknowledged_at_unix.saturating_mul(1000)),
                version_label(app_version),
                env!("CARGO_PKG_VERSION"),
                machine_clause(machine.as_deref())
            )),
            _ => None,
        }
    }
}

/// How the record's machine relates to this one, as a clause for a sentence.
///
/// A record with no machine mark (one written before the mark existed) says so rather
/// than claiming it was made here or elsewhere.
fn machine_clause(machine: Option<&str>) -> String {
    match machine.map(str::trim).filter(|mark| !mark.is_empty()) {
        Some(mark) => match crate::machine::relation(mark) {
            // O3: the record belongs to the configuration directory, and a directory
            // copied to a second person on this machine carries the first person's
            // confirmation with it. The clause names the configuration as the thing
            // that was confirmed, and the machine it was made on beside it, so a
            // second person can tell the record is not theirs.
            crate::machine::Relation::Here => {
                "this configuration's record was made on this machine".to_owned()
            }
            crate::machine::Relation::Elsewhere => format!(
                "this configuration's record was made on another machine ({mark}) — this machine has not recorded a confirmation"
            ),
            crate::machine::Relation::Unnamed | crate::machine::Relation::Undecidable => {
                "the record does not name the machine it was made on".to_owned()
            }
        },
        None => "the record does not name the machine it was made on".to_owned(),
    }
}

/// Write the record. The file is 0600 like the credentials file, and holds no secret.
pub fn record(path: &Path) -> Result<AckState, io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    // W32-6: the confirmation is made on one machine; the record carries which one, so
    // a copied directory does not present another machine's confirmation as this
    // machine's own.
    let machine = crate::machine::mark()
        .map(|mark| format!("machine = {}", toml_string(&mark)))
        .unwrap_or_default();
    let text = format!(
        "# Written once, when the risk of running an agent was confirmed.\n\
         # Removing this file makes the next start show the acknowledgment again.\n\
         acknowledged = true\n\
         acknowledged_at_unix = {stamp}\n\
         app_version = \"{}\"\n\
         {machine}\n",
        env!("CARGO_PKG_VERSION")
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(SECRET_FILE_MODE)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    set_mode(path, SECRET_FILE_MODE)?;
    Ok(AckState::read(path))
}

/// A TOML string literal for a value the record writes.
fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// The version a record carries, or `unknown` when the file did not say.
fn version_label(app_version: &str) -> &str {
    if app_version.trim().is_empty() {
        "unknown"
    } else {
        app_version.trim()
    }
}

/// The shape of the record on disk.
#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    acknowledged: bool,
    #[serde(default)]
    acknowledged_at_unix: u64,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    machine: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_fresh_installation_is_not_confirmed() {
        let root = scratch_root("risk-fresh");
        let state = AckState::read(&root.join("risk-acknowledgment.toml"));
        assert_eq!(state, AckState::Missing);
        assert!(!state.is_confirmed());
    }

    #[test]
    fn recording_makes_a_later_read_confirmed() {
        let root = scratch_root("risk-record");
        let path = root.join("risk-acknowledgment.toml");
        let state = record(&path).expect("the record is written");
        assert!(state.is_confirmed());
        assert!(AckState::read(&path).is_confirmed());
    }

    #[test]
    fn the_record_line_names_the_date_and_the_version_a_predecessor_agreed_to() {
        // C-3: a second person on a used configuration directory must be able to read
        // what was agreed, when, and under which version — read from the file itself,
        // never assumed to be the running version.
        let root = scratch_root("risk-line");
        let path = root.join("risk-acknowledgment.toml");
        fs::write(
            &path,
            "acknowledged = true\nacknowledged_at_unix = 1700000000\napp_version = \"0.1.7\"\n",
        )
        .expect("the test writes a predecessor's record");
        let state = AckState::read(&path);
        let line = state.record_line().expect("a record has a line");
        assert!(line.contains("the risk was acknowledged on"), "{line}");
        assert!(line.contains("0.1.7"), "the version the file carries: {line}");
        assert!(line.contains("press R"), "the way to read the terms again: {line}");
        // F3: the version now running is named beside the one the record carries, so
        // the comparison is on the screen instead of only in `--version`.
        assert!(
            line.contains(env!("CARGO_PKG_VERSION")),
            "the running version is not shown: {line}"
        );
        assert!(
            line.contains("this build is"),
            "the running version is not labelled: {line}"
        );
        // A missing record has no line to draw.
        assert!(AckState::Missing.record_line().is_none());
    }

    #[test]
    fn the_record_names_the_configuration_it_belongs_to() {
        // O3: a configuration directory copied to a second person on the same machine
        // read `recorded on this machine`, so the second person could not tell that a
        // different person agreed. The clause now names the configuration as the thing
        // the record belongs to, and the machine it was made on beside it.
        if let Some(mark) = crate::machine::mark() {
            let clause = machine_clause(Some(&mark));
            assert!(
                clause.contains("this configuration's record was made on this machine"),
                "{clause}"
            );
        }
        let elsewhere = machine_clause(Some("another-host-abc123"));
        assert!(
            elsewhere.contains("this configuration's record was made on another machine"),
            "{elsewhere}"
        );
        assert!(
            elsewhere.contains("this machine has not recorded a confirmation"),
            "{elsewhere}"
        );
        // A record that names no machine still says so, rather than claiming one.
        assert!(machine_clause(None).contains("does not name the machine"));
    }

    #[test]
    fn the_record_is_private_and_holds_no_secret() {
        let root = scratch_root("risk-mode");
        let path = root.join("risk-acknowledgment.toml");
        record(&path).expect("the record is written");
        let mode = fs::metadata(&path)
            .expect("the record exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let text = fs::read_to_string(&path).expect("readable by the test");
        assert!(!text.contains("token") && !text.contains("password"));
    }

    #[test]
    fn a_corrupt_record_is_not_a_confirmation() {
        let root = scratch_root("risk-corrupt");
        let path = root.join("risk-acknowledgment.toml");
        fs::write(&path, "this is not toml = = =").expect("the test writes the record");
        let state = AckState::read(&path);
        assert!(!state.is_confirmed());
        assert!(matches!(state, AckState::Unreadable { .. }));
    }

    #[test]
    fn a_record_that_says_no_is_not_a_confirmation() {
        let root = scratch_root("risk-no");
        let path = root.join("risk-acknowledgment.toml");
        fs::write(&path, "acknowledged = false\n").expect("the test writes the record");
        assert!(!AckState::read(&path).is_confirmed());
    }

    #[test]
    fn a_fresh_configuration_directory_shows_it_again() {
        // "Fresh installation" means an empty configuration directory: a record in one
        // directory says nothing about another.
        let first = scratch_root("risk-first");
        let second = scratch_root("risk-second");
        record(&first.join("risk-acknowledgment.toml")).expect("recorded once");
        assert!(AckState::read(&first.join("risk-acknowledgment.toml")).is_confirmed());
        assert!(!AckState::read(&second.join("risk-acknowledgment.toml")).is_confirmed());
    }

    #[test]
    fn the_text_names_the_facts_it_must_name() {
        let whole = format!("{TITLE}\n{}", LINES.join("\n"));
        assert!(whole.contains("runs as you"));
        assert!(whole.contains("not a sandbox"));
        assert!(whole.contains("credentials file"));
        assert!(whole.contains("Credentials screen"));
        assert!(whole.contains("confirm"));
        for line in LINES {
            assert!(
                line.chars().count() <= 72,
                "a line is too long to sit on one row: {line}"
            );
        }
    }
}
