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
    /// The word written in the transcript and shown on screen.
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
            // A run whose start is recorded but whose end is not is running, not
            // "unknown": its `plan.json` and opening transcript record are on disk.
            None => match self.started_at.as_deref() {
                Some(started) => match crate::time::parse_rfc3339(started) {
                    Some(start) if now_ms > start => {
                        format!("running ({}s)", (now_ms - start) / 1000)
                    }
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
    pub fn started_text(&self) -> String {
        match self.started_at.as_deref().and_then(crate::time::parse_rfc3339) {
            Some(start) => crate::time::compact_utc(start),
            None => "unknown start".to_owned(),
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
fn summarise(agent: &str, run_id: &str, run_dir: &Path) -> RunSummary {
    let log = run_dir.join(LOG_FILE);
    let Some(finished) = transcript::terminal_record(run_dir) else {
        return RunSummary {
            agent: agent.to_owned(),
            run_id: run_id.to_owned(),
            outcome: None,
            reason: None,
            duration_ms: None,
            // The opening record is written before the run does anything, so a run in
            // progress has a start time even though it has no terminal record yet.
            started_at: transcript::first_started_at(run_dir),
            log,
            dir: run_dir.to_path_buf(),
        };
    };
    if let Record::RunFinished {
        outcome,
        duration_ms,
        reason,
        ..
    } = finished
    {
        RunSummary {
            agent: agent.to_owned(),
            run_id: run_id.to_owned(),
            outcome: RunOutcome::parse(&outcome),
            reason,
            duration_ms: Some(duration_ms),
            started_at: transcript::first_started_at(run_dir),
            log,
            dir: run_dir.to_path_buf(),
        }
    } else {
        RunSummary {
            agent: agent.to_owned(),
            run_id: run_id.to_owned(),
            outcome: None,
            reason: None,
            duration_ms: None,
            started_at: transcript::first_started_at(run_dir),
            log,
            dir: run_dir.to_path_buf(),
        }
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
            log: PathBuf::from("/tmp/run.log"),
            dir: PathBuf::from("/tmp/run"),
        };
        assert_eq!(run.describe_at(1_012_000), "running (12s)");
        // A clock before the recorded start never prints a negative age.
        assert_eq!(run.describe_at(500), "running");
    }
}
