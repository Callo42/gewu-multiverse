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
    /// The declared shell steps the run executes, in sequence. A step that fails
    /// ends the run.
    pub steps: Vec<String>,
    /// How long the run may last before it is stopped. A bound, not a promise.
    pub timeout_secs: u64,
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
    /// The outcome and how long it took, for the runs screen.
    pub fn describe(&self) -> String {
        let outcome = match (self.outcome, &self.reason) {
            (Some(outcome), Some(reason)) => format!("{} ({reason})", outcome.as_str()),
            (Some(outcome), None) => outcome.as_str().to_owned(),
            (None, _) => "no terminal record".to_owned(),
        };
        match self.duration_ms {
            Some(ms) => format!("{outcome}, {:.1}s", ms as f64 / 1000.0),
            None => outcome,
        }
    }

    /// When the run started, as written in the transcript.
    pub fn started_text(&self) -> String {
        self.started_at
            .clone()
            .unwrap_or_else(|| "unknown start".to_owned())
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
            started_at: None,
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
        let text = r#"{"run_id":"r","agent":"a","workspace":"/w","instruction":"/i","skills_dir":"/s","steps":[],"timeout_secs":1,"extra":true}"#;
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
    fn a_run_without_a_terminal_record_is_listed_and_says_so() {
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
        assert!(runs[0].describe().contains("no terminal record"));
    }
}
