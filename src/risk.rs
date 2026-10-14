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
            } => format!("acknowledged at Unix time {acknowledged_at_unix}"),
            AckState::Unreadable { reason } => format!(
                "a record is there but cannot be read ({reason}); it is not treated as a confirmation"
            ),
        }
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
    let text = format!(
        "# Written once, when the risk of running an agent was confirmed.\n\
         # Removing this file makes the next start show the acknowledgment again.\n\
         acknowledged = true\n\
         acknowledged_at_unix = {stamp}\n\
         app_version = \"{}\"\n",
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

/// The shape of the record on disk.
#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    acknowledged: bool,
    #[serde(default)]
    acknowledged_at_unix: u64,
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
