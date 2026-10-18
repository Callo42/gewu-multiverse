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
use sha2::{Digest, Sha256};

use crate::mode::{SECRET_FILE_MODE, set_mode};

/// The heading of the acknowledgment screen.
///
/// It is also the first line of the terms text the digest covers, so it is never
/// changed for the review's sake.
pub const TITLE: &str = "Before the first agent runs";

/// The heading of the **read-only review** (`R`) when a record already exists.
///
/// w44 F3: the review kept `Before the first agent runs` while the record was on file
/// and an agent existed, so the overlay described a state the person was not in. The
/// review is the terms that were acknowledged, read back; this names that state.
/// It is the overlay's title only — [`terms_text`] keeps [`TITLE`].
pub const REVIEW_TITLE: &str = "The terms you acknowledged — read-only";

/// What a record can and cannot prove, stated where the record is read (C-4).
///
/// The Compliance walk asked the product to be honest about its own record: it is a
/// plain TOML file, there is no signature or MAC, and anyone who can read the
/// directory can edit it or write one. The product must not let a reader take it for
/// something it is not, so the limit is said in the review overlay and in the hub's
/// note area — the same sentence in both places.
pub const RECORD_LIMIT: &str = "This record is a plain file (mode 0600) that anyone who can read this directory can also edit; the product does not sign it, so it shows what was recorded, not that the product recorded it. Its digest only tells whether the terms text has changed since then.";

/// One text for the terms this build shows, and the digest the record carries.
///
/// The digest is of the text, not of the file, so formatting the record cannot make
/// it look as though the terms changed.
pub fn terms_text() -> String {
    format!("{TITLE}\n{}", LINES.join("\n"))
}

/// A SHA-256 digest of the terms text this build shows, as the record writes it.
///
/// C-4: the record is not tamper-evident — anyone with the file can rewrite both the
/// terms and this digest — but the digest is checkable, so the product can tell
/// whether the terms **it now shows** are the ones the record was made under. That is
/// the whole claim, and it is the claim the screens make.
pub fn terms_digest() -> String {
    let mut hasher = Sha256::new();
    hasher.update(terms_text().as_bytes());
    format!("sha256:{}", hex(hasher.finalize().as_slice()))
}

/// Lower-case hex, for a digest a person can compare by hand.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

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
        /// The digest of the terms text the record was made under (C-4).
        ///
        /// `None` for a record written before the digest existed. The product checks
        /// it against the terms it now shows and says plainly that a match only means
        /// the text has not changed — the file is not signed and anyone can rewrite
        /// both the terms and the digest.
        terms_digest: Option<String>,
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
                terms_digest: record.terms_digest,
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
        self.record_line_with(None)
    }

    /// [`AckState::record_line`] with one extra clause in the version parenthetical.
    ///
    /// U3: the comparison the line already makes (the record's version against this
    /// build) is joined by what the configuration has been used by since, so an older
    /// build reading newer data has its own word rather than silence.
    pub fn record_line_with(&self, extra: Option<&str>) -> Option<String> {
        let extra = extra.map(|clause| format!("; {clause}")).unwrap_or_default();
        match self {
            AckState::Recorded {
                acknowledged_at_unix,
                app_version,
                machine,
                ..
            } => Some(format!(
                "the risk was acknowledged on {} (version {}; this build is {}{}; {}) — press R to read the terms again; the record is not changed",
                crate::time::compact_utc(acknowledged_at_unix.saturating_mul(1000)),
                version_label(app_version),
                env!("CARGO_PKG_VERSION"),
                extra,
                machine_clause(machine.as_deref())
            )),
            _ => None,
        }
    }

    /// What the record's digest can and cannot say, checked against the terms this
    /// build shows (C-4).
    ///
    /// Three answers, all honest: the digest matches, the digest differs (the terms
    /// text changed), or the record carries no digest (it was written before the
    /// digest existed). None of them claims the file was written by the product.
    pub fn terms_check(&self) -> Option<String> {
        let AckState::Recorded {
            terms_digest: stored_field,
            ..
        } = self
        else {
            return None;
        };
        let stored_digest = stored_field.as_deref().map(str::trim);
        Some(match stored_digest {
            Some(stored) if stored == terms_digest() => {
                "the digest this record carries matches the terms shown here: the text has not changed since it was recorded".to_owned()
            }
            Some(_) => {
                "the digest this record carries does not match the terms shown here: the terms text has changed since it was recorded".to_owned()
            }
            None => {
                "this record carries no digest of the terms, so the product cannot tell whether the terms have changed since it was made".to_owned()
            }
        })
    }

    /// What to do about an acknowledgment record made on another machine (w45 F2).
    ///
    /// The [`machine_clause`] sentence says what the product knows (the record names
    /// another machine) and what it does not (this machine has recorded no
    /// confirmation), but after moving a `HOME` a person was left unsure whether
    /// anything could be done. This names the consequence — starting here is still
    /// allowed under that record — and the one action that makes this machine's own
    /// record: remove the record file and start again, which shows the acknowledgment
    /// once more. `None` in every other relation, so the sentence is added only where
    /// it is true.
    #[must_use]
    pub fn elsewhere_action(&self, record_path: &Path) -> Option<String> {
        let AckState::Recorded { machine, .. } = self else {
            return None;
        };
        let mark = machine
            .as_deref()
            .map(str::trim)
            .filter(|mark| !mark.is_empty())?;
        if crate::machine::relation(mark) != crate::machine::Relation::Elsewhere {
            return None;
        }
        Some(format!(
            "starting an agent here is still allowed under that record; to record this machine's own confirmation, remove {} and start again",
            record_path.display()
        ))
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
            crate::machine::Relation::Elsewhere => {
                "this configuration's record was made on another machine — this machine has not recorded a confirmation".to_owned()
            }
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
         # This file is not signed or tamper-evident: anyone who can read it can edit\n\
         # it. terms_digest lets the product check only whether the terms text has\n\
         # changed since it was written (C-4).\n\
         acknowledged = true\n\
         acknowledged_at_unix = {stamp}\n\
         app_version = \"{}\"\n\
         terms_digest = {}\n\
         {machine}\n",
        env!("CARGO_PKG_VERSION"),
        toml_string(&terms_digest())
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
    #[serde(default)]
    terms_digest: Option<String>,
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
    fn the_review_title_is_the_overlays_not_the_terms_text() {
        // w44 F3 added a title for the read-only review; the terms text the digest is
        // computed over must not move with it, or every existing record would read as
        // "the terms text has changed".
        assert_ne!(TITLE, REVIEW_TITLE);
        let text = terms_text();
        assert!(text.starts_with(TITLE), "{text}");
        assert!(
            !text.contains(REVIEW_TITLE),
            "the review title is inside the digested terms text: {text}"
        );
    }

    #[test]
    fn a_record_from_another_machine_names_the_way_back() {
        // w45 F2: the Elsewhere clause says what is known and what is not; the action
        // says what to do about it. No host in this test reads as the foreign mark.
        let state = AckState::Recorded {
            acknowledged_at_unix: 1_700_000_000,
            app_version: "0.1.22".to_owned(),
            machine: Some("gewu-other-host/0000000000000000000000000000abcd".to_owned()),
            terms_digest: None,
        };
        if crate::machine::mark().is_none() {
            return;
        }
        let action = state
            .elsewhere_action(Path::new("/somewhere/risk-acknowledgment.toml"))
            .expect("a foreign record has an action");
        assert!(action.contains("still allowed"), "{action}");
        assert!(
            action.contains("/somewhere/risk-acknowledgment.toml") && action.contains("start again"),
            "{action}"
        );
        // A record with no machine has no move to explain, and says nothing extra.
        let unnamed = AckState::Recorded {
            acknowledged_at_unix: 1_700_000_000,
            app_version: "0.1.22".to_owned(),
            machine: None,
            terms_digest: None,
        };
        assert!(unnamed
            .elsewhere_action(Path::new("/x/risk-acknowledgment.toml"))
            .is_none());
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
