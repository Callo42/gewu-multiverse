//! What a run is: its identity, the plan it was started with, and how it ended.
//!
//! A run is one execution of one agent's process. It has a run id — a name that is
//! also a directory — a plan written before the process starts, and exactly one
//! outcome, decided from what the process did rather than from what it wrote.
//!
//! The records of a run live in its own directory under the state root:
//!
//! ```text
//! <state root>/<agent>/<run id>/
//!     plan.json         what the run was started with, written before it starts
//!     run.log           the agent process's own output, and the tool's own lines
//!     transcript.jsonl  the append-only record of what happened
//!     skills/           the skill set this run had installed
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::naming::is_safe_agent_name;
use crate::time::{now_unix_ms, rfc3339};
use crate::transcript::{self, Record};

/// The prefix every run directory has, so a stray directory is never mistaken for one.
pub const RUN_ID_PREFIX: &str = "run-";

/// A fresh run id: a timestamp and a sequence number.
///
/// The timestamp comes first so that sorting run ids sorts runs by age. The sequence
/// keeps two runs of the same millisecond apart, and keeps the whole thing a single
/// safe path component of a fixed, modest length.
pub fn new_run_id(sequence: u64) -> String {
    format!("{RUN_ID_PREFIX}{}-{:04}", now_unix_ms(), sequence % 10_000)
}

/// Whether a directory name looks like a run id.
pub fn looks_like_run_id(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(RUN_ID_PREFIX) else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// The instant a run id was made, when its shape carries one.
///
/// A run id begins with the millisecond clock the supervisor made it at, so a run
/// whose opening transcript record has not landed yet still has a start time on disk
/// — in its own directory name. Z-1 reads it so the Runs screen can say `running`
/// from the first frame the hub says it, instead of `unknown start`.
pub fn started_ms_from_run_id(run_id: &str) -> Option<u64> {
    let rest = run_id.strip_prefix(RUN_ID_PREFIX)?;
    let (millis, _sequence) = rest.split_once('-')?;
    millis.parse().ok()
}

/// The one word for a run in flight, on every screen.
///
/// The hub (`State::word`) and the Runs row (`RunSummary::describe_at`) both print
/// this, so the first seconds of a run cannot be `running` on one screen and
/// `unknown start` on the other (Z-1).
pub fn running_word(elapsed_ms: u64) -> String {
    format!("running ({}s)", elapsed_ms / 1000)
}

/// The one word for a record that is present but cannot be read (W38-1, W41-2).
///
/// A run `unreadable` is not a run that never happened: the word says the product
/// cannot read the record, never that there is no run. The hub's state column and
/// the Runs row print this same string, so a corrupt transcript cannot read as
/// `idle` ("no run") on the hub while Runs says `unreadable`.
pub const UNREADABLE_WORD: &str = "unreadable";

/// Everything a run needs to know, written down before it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    /// The identity of this run.
    pub run_id: String,
    /// The agent the run belongs to.
    pub agent: String,
    /// The agent's own working directory.
    pub workspace: PathBuf,
    /// The instruction document the run reads.
    pub instruction: PathBuf,
    /// The directory holding the skills the run installs.
    pub skills_dir: PathBuf,
    /// The endpoint the agent's configuration names, called directly by the run.
    pub endpoint: String,
    /// The model id the agent's configuration names.
    pub model: String,
    /// The credentials file the model key is read from. It holds no key itself.
    pub credentials: PathBuf,
    /// The declared bound on model turns. Reaching it ends the run, visibly.
    pub max_turns: u32,
    /// The declared shell steps the run executes first, in sequence. A step that
    /// fails ends the run.
    pub steps: Vec<String>,
    /// How long the run may last before it is stopped. A bound, not a promise.
    pub timeout_secs: u64,
}

/// The model an agent runs against, as the configuration describes it.
///
/// This is what the screens and the supervisor carry from an agent's configuration to
/// the plan of a run: where to call, which model, and how many turns are allowed. The
/// key is deliberately not here — it is read from the credentials file by the run
/// itself, at the moment of the call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunModel {
    /// The endpoint to call directly.
    pub endpoint: String,
    /// The model id to ask for.
    pub model: String,
    /// The bound on model turns in one run.
    pub max_turns: u32,
}

impl RunPlan {
    /// Write the plan into the run directory. The file is private, like the rest.
    pub fn write(&self, run_dir: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        crate::mode::write_private(run_dir.join(PLAN_FILE), text.as_bytes())
    }

    /// Read the plan back out of the run directory.
    pub fn read(run_dir: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(run_dir.join(PLAN_FILE))?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    /// The declared steps, read from the agent's steps file: one shell command per
    /// line, blank lines and lines starting with `#` skipped.
    pub fn read_steps(path: &Path) -> io::Result<Vec<String>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(source),
        };
        Ok(parse_steps(&text))
    }
}

/// The plan file's name.
pub const PLAN_FILE: &str = "plan.json";

/// The log file's name.
pub const LOG_FILE: &str = "run.log";

/// The note, in an agent's state directory, that lists the run records which existed
/// before the current agent was created under that name.
///
/// A run record names its agent by name. When an agent is removed and a new agent is
/// created with the same name, the old records are still on disk but they are not the
/// new agent's history. This note is the boundary: it holds run ids only, it is not a
/// run record, and it never changes a record. An agent created before this note
/// existed has no note, so its records stay its own as before.
pub const PREVIOUS_RUNS_FILE: &str = "previous-runs";

/// Parse a steps document: one shell command per line, comments and blanks skipped.
pub fn parse_steps(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .map(str::to_owned)
        .collect()
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The agent process exited cleanly and every declared step succeeded.
    Succeeded,
    /// The agent process failed, or a declared step did.
    Failed,
    /// The run was stopped on request before it ended on its own.
    Stopped,
    /// The run reached its timeout and was stopped for that reason.
    TimedOut,
}

impl RunOutcome {
    /// Every outcome, in the order a screen may list them.
    ///
    /// The vocabulary guard drives the screens over this list, so a new outcome cannot
    /// be added without a word on every screen (W39-2).
    pub const ALL: [RunOutcome; 4] = [
        RunOutcome::Succeeded,
        RunOutcome::Failed,
        RunOutcome::Stopped,
        RunOutcome::TimedOut,
    ];

    /// The one word for this outcome, written in the transcript and shown on screen.
    ///
    /// This is the product's vocabulary map: the hub, the Runs list and the Runs note
    /// all read this word, so one outcome cannot be `timed_out` in one place and
    /// `timed out` in another (W39-2).
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Succeeded => "succeeded",
            RunOutcome::Failed => "failed",
            RunOutcome::Stopped => "stopped",
            RunOutcome::TimedOut => "timed_out",
        }
    }

    /// Read the word back. An unknown word is `None`, never a guess.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "succeeded" => Some(RunOutcome::Succeeded),
            "failed" => Some(RunOutcome::Failed),
            "stopped" => Some(RunOutcome::Stopped),
            "timed_out" => Some(RunOutcome::TimedOut),
            _ => None,
        }
    }

    /// Whether the run went the way it was meant to.
    ///
    /// C-3: `succeeded` is the only outcome that is not something to explain. The
    /// hub's note area names the reason for **every** other outcome, not only
    /// `failed`, so `timed_out` and `stopped` are not left without their reason on
    /// the landing screen.
    pub fn is_success(self) -> bool {
        matches!(self, RunOutcome::Succeeded)
    }
}

/// One run, as the runs screen shows it: read from the records on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The agent the run belongs to.
    pub agent: String,
    /// The run's identity.
    pub run_id: String,
    /// How it ended, when a terminal record is there.
    pub outcome: Option<RunOutcome>,
    /// Why it ended that way, when the record says.
    pub reason: Option<String>,
    /// How long it lasted, when the record says.
    pub duration_ms: Option<u64>,
    /// When it started, as written in the transcript.
    pub started_at: Option<String>,
    /// The reason the run's own transcript could not be read, when it is present but
    /// unreadable — as distinct from a file that is not there (W38-1).
    pub read_error: Option<String>,
    /// The run's log file.
    pub log: PathBuf,
    /// The run's directory.
    pub dir: PathBuf,
}

impl RunSummary {
    /// The outcome and how long it took, for the runs table.
    ///
    /// Short enough not to be clipped in a narrow table: the run's own reason is shown
    /// in the note area beside the table, where it can wrap. A run that is still going
    /// says how long it has been going, from the start time its own record carries —
    /// the one thing a person returning to the screen wants and the finish-state
    /// column already knows how to render.
    pub fn describe(&self) -> String {
        self.describe_at(crate::time::now_unix_ms())
    }

    /// [`RunSummary::describe`] against a chosen clock, so the elapsed time is testable.
    pub fn describe_at(&self, now_ms: u64) -> String {
        match self.outcome {
            Some(outcome) => {
                let outcome = outcome.as_str().to_owned();
                match self.duration_ms {
                    Some(ms) => format!("{outcome}, {:.1}s", ms as f64 / 1000.0),
                    None => outcome,
                }
            }
            // A record that is present but unreadable is not a run whose ending was
            // never written: it is one whose ending the product cannot read (W38-1).
            None if self.read_error.is_some() => UNREADABLE_WORD.to_owned(),
            // A run whose start is recorded but whose end is not is running, not
            // "unknown": its `plan.json` and opening transcript record are on disk.
            None => match self.started_at.as_deref() {
                Some(started) => match crate::time::parse_rfc3339(started) {
                    Some(start) if now_ms >= start => running_word(now_ms - start),
                    _ => "running".to_owned(),
                },
                None => "no terminal record".to_owned(),
            },
        }
    }

    /// When the run started, as written in the transcript.
    /// When the run started, as a compact UTC stamp that fits the table.
    ///
    /// The full record keeps its milliseconds and its `Z`; the table needs a form
    /// that is not clipped before the `Z`, so the clock on screen is never ambiguous
    /// (W-4).
    ///
    /// A run whose opening record cannot be read still has a start: the run id is
    /// made from the instant the supervisor made it (`run::started_ms_from_run_id`).
    /// C-2: `unknown start` was printed in the Runs `started (UTC)` column for a
    /// present-but-unreadable record even though the id carried the instant the live
    /// path already reads. The id is the last fallback before `unknown start`, which
    /// is now reserved for an id that carries no clock at all.
    pub fn started_text(&self) -> String {
        match self.started_at.as_deref().and_then(crate::time::parse_rfc3339) {
            Some(start) => crate::time::compact_utc(start),
            None => match started_ms_from_run_id(&self.run_id) {
                Some(ms) => crate::time::compact_utc(ms),
                None => "unknown start".to_owned(),
            },
        }
    }
}

/// The most recent runs of every named agent, newest first, at most `limit`.
///
/// This reads the records rather than remembering anything: a run started in an
/// earlier session is listed just like one started now. A run with no terminal
/// record is listed too, and says so.
pub fn recent_runs(state_root: &Path, agents: &[String], limit: usize) -> Vec<RunSummary> {
    let mut all: Vec<RunSummary> = Vec::new();
    for agent in agents {
        if !is_safe_agent_name(agent) {
            continue;
        }
        let mut summaries = runs_of_agent(state_root, agent);
        all.append(&mut summaries);
    }
    // Newest first. A run id starts with its timestamp, so the id sorts by age.
    all.sort_by(|left, right| right.run_id.cmp(&left.run_id));
    all.truncate(limit);
    all
}

/// The agent directory names under the state root that the configuration no longer
/// lists.
///
/// R-5: the Runs note area counted a removed agent's records but the list did not
/// show them, so their transcript and log could not be opened in-app until the agent
/// was re-added. The list is now built from these names too, and a record whose agent
/// is not configured is marked in the table, so the count and the rows agree.
pub fn unconfigured_agents(state_root: &Path, configured: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = fs::read_dir(state_root) else {
        return names;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !configured.iter().any(|known| known == &name) {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// The run ids the agent's state directory calls earlier agents' records.
///
/// The file holds one run id per line. Anything that is not a run id is ignored, so a
/// hand-edited or partly written note cannot make a directory look like a run.
pub fn previous_run_ids(state_dir: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(state_dir.join(PREVIOUS_RUNS_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| looks_like_run_id(line))
        .map(str::to_owned)
        .collect()
}

/// Write, beside an agent's state directory, the run records that predate this agent.
///
/// Called when an agent is created. Every run directory already under the name is
/// added to the note, together with any ids the note already carries, so removing a
/// record and creating the name again does not lose the boundary. No run record is
/// read, rewritten or created; the note is the product's own small identity file.
pub fn mark_existing_runs_previous(state_dir: &Path) -> io::Result<()> {
    let mut ids = previous_run_ids(state_dir);
    if let Ok(entries) = fs::read_dir(state_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(run_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path.is_dir() && looks_like_run_id(run_id) {
                ids.push(run_id.to_owned());
            }
        }
    }
    ids.sort();
    ids.dedup();
    let mut text = ids.join("\n");
    if !ids.is_empty() {
        text.push('\n');
    }
    crate::mode::write_private(state_dir.join(PREVIOUS_RUNS_FILE), text.as_bytes())
}

/// Every run recorded for one agent, in no particular order.
pub fn runs_of_agent(state_root: &Path, agent: &str) -> Vec<RunSummary> {
    let dir = state_root.join(agent);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return Vec::new(),
    };
    let mut summaries = Vec::new();
    for entry in entries.flatten() {
        let run_dir = entry.path();
        let Some(run_id) = run_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !run_dir.is_dir() || !looks_like_run_id(run_id) {
            continue;
        }
        summaries.push(summarise(agent, run_id, &run_dir));
    }
    summaries
}

/// Read one run's records into a summary.
///
/// The transcript is read once, and a read that fails on a file that is **there** is
/// carried in `read_error` rather than swallowed: a screen must be able to say the
/// record could not be read instead of calling it a run with no terminal record
/// (W38-1).
fn summarise(agent: &str, run_id: &str, run_dir: &Path) -> RunSummary {
    let log = run_dir.join(LOG_FILE);
    let (file, read_error) = match transcript::read_checked(run_dir) {
        Ok(file) => (file, None),
        Err(source) => (None, Some(source.to_string())),
    };
    let started_at = file.as_ref().and_then(|file| {
        file.records.iter().find_map(|record| match record {
            Record::RunStarted { ts, .. } => Some(ts.clone()),
            _ => None,
        })
    });
    let finished = file.as_ref().and_then(|file| {
        file.records
            .iter()
            .rev()
            .find(|record| matches!(record, Record::RunFinished { .. }))
            .cloned()
    });
    let (outcome, reason, duration_ms) = match finished {
        Some(Record::RunFinished {
            outcome,
            duration_ms,
            reason,
            ..
        }) => (RunOutcome::parse(&outcome), reason, Some(duration_ms)),
        _ => (None, None, None),
    };
    RunSummary {
        agent: agent.to_owned(),
        run_id: run_id.to_owned(),
        outcome,
        reason,
        duration_ms,
        started_at,
        read_error,
        log,
        dir: run_dir.to_path_buf(),
    }
}

/// One wording for one fate of a file a run left behind (W38-1, W38-3).
///
/// Three cases, three sentences, wherever the file is named:
///
/// - the file is there and shows its **size**;
/// - the file is **not there**, so it is `gone, the record has no <kind>`;
/// - the file is there but its size **cannot be read** (permission denied), or a
///   caller passes a content error in `read_error` (`transcript.jsonl` that is not
///   JSONL), and then it says the reason.
///
/// The three cases are decided in one place so the Runs note and the Agent screen
/// cannot call the same missing file two different things.
pub fn file_sentence(path: &Path, kind: &str, read_error: Option<&str>) -> String {
    if let Some(reason) = read_error {
        return format!("{} — the record could not be read: {reason}", path.display());
    }
    // U1: `fs::metadata` succeeds for a directory, so a `run.log` that is a
    // directory was drawn as a readable file with a size (`4.0 KiB`) and `o` then
    // said it "is not there yet". The directory case is answered by reading it (a
    // directory answers `Is a directory (os error 21)`); a plain file that cannot be
    // opened answers `Permission denied (os error 13)`; a readable file shows its
    // size. One order, so no path is called readable before it is opened.
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return format!("{} — gone, the record has no {kind}", path.display());
        }
        Err(source) => return could_not_read(path, &source),
    };
    if meta.is_dir() {
        return match fs::read(path) {
            Err(source) => could_not_read(path, &source),
            Ok(_) => format!(
                "{} ({})",
                path.display(),
                crate::app::filesize_text(meta.len())
            ),
        };
    }
    match fs::File::open(path) {
        Ok(_) => format!(
            "{} ({})",
            path.display(),
            crate::app::filesize_text(meta.len())
        ),
        Err(source) => could_not_read(path, &source),
    }
}

/// The one sentence for a path that is there but cannot be read, with the reason.
///
/// The wording the whole product uses for a present-but-unreadable file (W38-1,
/// Z-3, U1): `— the record could not be read: <reason>`. Keeping it in one place is
/// what stops a directory, a permission denial and a broken link being called three
/// different things on three screens.
pub fn could_not_read(path: &Path, source: &io::Error) -> String {
    format!("{} — the record could not be read: {source}", path.display())
}

/// Whether a path is a regular, readable file — the one case in which the product
/// can open it in a pager or show its size as content (U1).
///
/// `is_file()` alone is not enough: a directory passes `metadata` and a file the
/// mode forbids reading passes `is_file`. The open decides it.
pub fn is_readable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => fs::File::open(path).is_ok(),
        _ => false,
    }
}

/// The instant a run started, as a timestamp string, for a log line.
pub fn started_stamp() -> String {
    rfc3339(now_unix_ms())
}

/// What the run records occupy on disk, and which of them the configuration no longer
/// lists.
///
/// The list of runs is capped at fifty so a screen stays a screen; the *records* are
/// not capped, they accumulate for ever. This measurement is what lets the product
/// say how much room they take and how many are not on the list, instead of leaving
/// a person to find the state directory by hand (L-1, L-2, L-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunFootprint {
    /// Every run directory under the state root, configured or not.
    pub records: usize,
    /// The bytes those directories occupy, recursively.
    pub bytes: u64,
    /// How many of `records` belong to an agent the configuration no longer lists.
    pub unconfigured_records: usize,
}

/// Measure every run record under `state_root`, not only the configured agents'.
///
/// Reading *every* agent directory is deliberate: a removed agent's records stay on
/// disk, and a screen that only counted the configured agents would hide them (L-4).
pub fn state_footprint(state_root: &Path, configured: &[String]) -> RunFootprint {
    let mut footprint = RunFootprint::default();
    let entries = match fs::read_dir(state_root) {
        Ok(entries) => entries,
        Err(_) => return footprint,
    };
    for agent_entry in entries.flatten() {
        let agent_dir = agent_entry.path();
        if !agent_dir.is_dir() {
            continue;
        }
        let agent = agent_entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .unwrap_or_default();
        let known = configured.iter().any(|name| name == &agent);
        let runs = match fs::read_dir(&agent_dir) {
            Ok(runs) => runs,
            Err(_) => continue,
        };
        for run_entry in runs.flatten() {
            let run_dir = run_entry.path();
            let Some(run_id) = run_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !run_dir.is_dir() || !looks_like_run_id(run_id) {
                continue;
            }
            footprint.records += 1;
            footprint.bytes = footprint.bytes.saturating_add(directory_bytes(&run_dir));
            if !known {
                footprint.unconfigured_records += 1;
            }
        }
    }
    footprint
}

/// The bytes one directory occupies, recursively. Symbolic links are not followed, so
/// a record that links elsewhere cannot make the measurement loop.
fn directory_bytes(path: &Path) -> u64 {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path()));
        } else if let Ok(metadata) = entry.metadata() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use crate::transcript::Transcript;

    fn plan(run_id: &str, agent: &str) -> RunPlan {
        RunPlan {
            run_id: run_id.to_owned(),
            agent: agent.to_owned(),
            workspace: PathBuf::from("/tmp/ws"),
            instruction: PathBuf::from("/tmp/i.md"),
            skills_dir: PathBuf::from("/tmp/skills"),
            endpoint: "https://model.example.org/v1".to_owned(),
            model: "example-model".to_owned(),
            credentials: PathBuf::from("/tmp/credentials.toml"),
            max_turns: 4,
            steps: vec!["echo one".to_owned(), "echo two".to_owned()],
            timeout_secs: 30,
        }
    }

    #[test]
    fn a_run_id_is_a_safe_directory_name_and_sorts_by_age() {
        let first = new_run_id(1);
        let second = new_run_id(2);
        assert!(looks_like_run_id(&first), "{first}");
        assert!(
            crate::naming::is_safe_agent_name(&first),
            "{first} must be one safe path component"
        );
        assert!(second > first, "{second} must sort after {first}");
    }

    #[test]
    fn a_stray_directory_is_not_mistaken_for_a_run() {
        assert!(!looks_like_run_id("skills"));
        assert!(!looks_like_run_id("run-"));
        assert!(!looks_like_run_id("run-abc"));
        assert!(looks_like_run_id("run-1760392800-0001-4711"));
    }

    #[test]
    fn a_plan_survives_a_round_trip_through_its_file() {
        let root = scratch_root("plan");
        let run_dir = root.join("run-1");
        fs::create_dir_all(&run_dir).expect("the run directory is created");
        let written = plan("run-1", "scribe");
        written.write(&run_dir).expect("the plan is written");
        let read = RunPlan::read(&run_dir).expect("the plan is read");
        assert_eq!(read, written);
    }

    #[test]
    fn a_plan_with_an_unknown_field_is_refused() {
        let text = r#"{"run_id":"r","agent":"a","workspace":"/w","instruction":"/i","skills_dir":"/s","endpoint":"http://e","model":"m","credentials":"/c","max_turns":1,"steps":[],"timeout_secs":1,"extra":true}"#;
        assert!(serde_json::from_str::<RunPlan>(text).is_err());
    }

    #[test]
    fn steps_skip_blank_lines_and_comments() {
        let steps = parse_steps("# a comment\n\necho one\n   \n  # indented\necho two\n");
        assert_eq!(steps, vec!["echo one", "echo two"]);
    }

    #[test]
    fn a_missing_steps_file_declares_no_steps() {
        let root = scratch_root("steps-missing");
        let steps =
            RunPlan::read_steps(&root.join("nothing.txt")).expect("no steps is not an error");
        assert!(steps.is_empty());
    }

    #[test]
    fn an_outcome_round_trips_through_its_word() {
        for outcome in [
            RunOutcome::Succeeded,
            RunOutcome::Failed,
            RunOutcome::Stopped,
            RunOutcome::TimedOut,
        ] {
            assert_eq!(RunOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(RunOutcome::parse("half-done"), None);
    }

    #[test]
    fn recent_runs_are_read_back_from_the_records_newest_first() {
        let root = scratch_root("recent-runs");
        let agent = "scribe";
        for (run_id, outcome) in [
            ("run-100-0001-1", RunOutcome::Succeeded),
            ("run-200-0002-1", RunOutcome::Stopped),
        ] {
            let dir = root.join(agent).join(run_id);
            fs::create_dir_all(&dir).expect("the run directory is created");
            let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
            transcript
                .append(&Record::run_started(run_id, agent, 42, "/w"))
                .expect("a record is appended");
            transcript
                .append(&Record::run_finished(run_id, outcome, Some(0), 12, None))
                .expect("a record is appended");
        }
        let runs = recent_runs(&root, &[agent.to_owned()], 10);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run_id, "run-200-0002-1");
        assert_eq!(runs[0].outcome, Some(RunOutcome::Stopped));
        assert_eq!(runs[1].outcome, Some(RunOutcome::Succeeded));
    }

    #[test]
    fn the_footprint_counts_every_record_and_its_room() {
        // L-1/L-2/L-4: the list is capped, but the records are not. The footprint
        // measures every run directory — including one whose agent the configuration
        // no longer lists — and its room on disk, so a screen can say what accumulates
        // instead of hiding it.
        let root = scratch_root("footprint");
        for (agent, ids) in [
            ("scribe", vec!["run-100-0001-1", "run-200-0002-1"]),
            ("ghost", vec!["run-300-0003-1"]),
        ] {
            for id in ids {
                let dir = root.join(agent).join(id);
                fs::create_dir_all(&dir).expect("the run directory is created");
                fs::write(dir.join("run.log"), vec![b'x'; 10]).expect("a log is written");
            }
        }
        // A directory that is not a run id is not a record.
        fs::create_dir_all(root.join("scribe").join("skills")).expect("a stray directory");
        let footprint = state_footprint(&root, &["scribe".to_owned()]);
        assert_eq!(footprint.records, 3, "every record is counted");
        assert_eq!(
            footprint.unconfigured_records, 1,
            "the removed agent's record is named"
        );
        assert!(
            footprint.bytes >= 30,
            "the room is measured: {}",
            footprint.bytes
        );
    }

    #[test]
    fn a_run_without_a_terminal_record_is_listed_as_running_not_unknown() {
        let root = scratch_root("recent-unfinished");
        let dir = root.join("scribe").join("run-300-0003-1");
        fs::create_dir_all(&dir).expect("the run directory is created");
        let mut transcript = Transcript::open(&dir).expect("a transcript is opened");
        transcript
            .append(&Record::run_started("run-300-0003-1", "scribe", 7, "/w"))
            .expect("a record is appended");
        let runs = recent_runs(&root, &["scribe".to_owned()], 10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, None);
        // The opening record is on disk, so the run is in progress, not unknown.
        assert!(runs[0].describe().contains("running"), "{}", runs[0].describe());
        assert!(runs[0].started_at.is_some());
    }

    #[test]
    fn a_run_directory_with_no_records_at_all_says_it_has_no_terminal_record() {
        let root = scratch_root("recent-empty-dir");
        let dir = root.join("scribe").join("run-300-0004-1");
        fs::create_dir_all(&dir).expect("the run directory is created");
        let runs = recent_runs(&root, &["scribe".to_owned()], 10);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].describe().contains("no terminal record"));
    }

    #[test]
    fn a_run_in_progress_reports_how_long_it_has_been_going() {
        // X-5: `running` alone leaves the one question the screen answers.
        let run = RunSummary {
            agent: "scribe".to_owned(),
            run_id: "run-100-0001-1".to_owned(),
            outcome: None,
            reason: None,
            duration_ms: None,
            started_at: Some(crate::time::rfc3339(1_000_000)),
            read_error: None,
            log: PathBuf::from("/tmp/run.log"),
            dir: PathBuf::from("/tmp/run"),
        };
        assert_eq!(run.describe_at(1_012_000), "running (12s)");
        // A clock before the recorded start never prints a negative age.
        assert_eq!(run.describe_at(500), "running");
    }

    #[test]
    fn one_word_for_one_outcome_on_every_screen() {
        // W39-2: the hub printed `timed out` while the Runs screen printed
        // `timed_out` for the same record, against the code's own "one word for one
        // outcome, on every screen". The guard drives the code's own vocabulary map:
        // every outcome's word round-trips through `parse`, the hub's `State::word`
        // is exactly that word, and the Runs screen's `describe` is the same string.
        for outcome in RunOutcome::ALL {
            assert_eq!(
                RunOutcome::parse(outcome.as_str()),
                Some(outcome),
                "the word {:?} does not round-trip",
                outcome.as_str()
            );
            let state = crate::supervisor::State::Finished {
                run_id: "run-1".to_owned(),
                outcome,
                reason: None,
                duration_ms: 0,
            };
            assert_eq!(
                state.word(),
                outcome.as_str(),
                "the hub and the Runs screen disagree about {outcome:?}"
            );
            let summary = RunSummary {
                agent: "scribe".to_owned(),
                run_id: "run-1".to_owned(),
                outcome: Some(outcome),
                reason: None,
                duration_ms: None,
                started_at: None,
                read_error: None,
                log: PathBuf::new(),
                dir: PathBuf::new(),
            };
            assert_eq!(
                summary.describe(),
                outcome.as_str(),
                "the Runs screen does not print the outcome's one word"
            );
        }
    }

    #[test]
    fn a_present_transcript_that_cannot_be_read_is_not_a_run_with_no_ending() {
        // W38-1: a corrupt transcript was reported as `no terminal record` with the
        // file still advertised at a normal size. The summary now carries the read
        // error, and the word is about the record, not about the run's ending.
        let root = scratch_root("unreadable-transcript");
        let dir = root.join("scribe").join("run-300-0005-1");
        fs::create_dir_all(&dir).expect("the run directory is created");
        fs::write(
            dir.join(crate::transcript::TRANSCRIPT_FILE),
            b"this line is not a record\n",
        )
        .expect("a corrupt transcript is written");
        let runs = recent_runs(&root, &["scribe".to_owned()], 10);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].read_error.is_some(), "the read error is dropped");
        assert_eq!(runs[0].describe(), UNREADABLE_WORD);
    }

    #[test]
    fn one_word_for_a_run_in_flight_and_for_a_record_that_cannot_be_read() {
        // Z-1 / W41-2: the state vocabulary is the code's own, on the hub and on Runs.
        // A run the hub calls `running (Ns)` must read the same on the Runs row, and a
        // record the product cannot read must not be `idle` (the word for no run) on
        // one screen while the other says `unreadable`.
        let now = crate::time::now_unix_ms();
        for elapsed in [0u64, 1_500, 61_000] {
            let state = crate::supervisor::State::Running {
                run_id: "run-1".to_owned(),
                pid: 1,
                elapsed_ms: elapsed,
            };
            let mut summary = RunSummary {
                agent: "scribe".to_owned(),
                run_id: "run-1".to_owned(),
                outcome: None,
                reason: None,
                duration_ms: None,
                started_at: Some(crate::time::rfc3339(now.saturating_sub(elapsed))),
                read_error: None,
                log: PathBuf::new(),
                dir: PathBuf::new(),
            };
            assert_eq!(state.word(), running_word(elapsed));
            assert_eq!(
                state.word(),
                summary.describe_at(now),
                "the hub and the Runs row disagree about a run in flight"
            );
            // A present-but-unreadable record is one word on both screens, and it is
            // never the word for no run.
            summary.started_at = None;
            summary.read_error = Some("not a record".to_owned());
            let unreadable = crate::supervisor::State::Unreadable {
                run_id: "run-1".to_owned(),
                reason: "not a record".to_owned(),
            };
            assert_eq!(unreadable.word(), UNREADABLE_WORD);
            assert_eq!(unreadable.word(), summary.describe_at(now));
            assert_ne!(unreadable.word(), crate::supervisor::State::Idle.word());
        }
    }
}
