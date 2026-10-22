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

use crate::time::now_unix_ms;
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
    /// How the run's shell is confined: `bwrap`, `unshare`, `landlock` or `none`.
    /// An older plan without the field is `none`: a run recorded before confinement
    /// existed is not retroactively promised it.
    #[serde(default)]
    pub confinement: String,
    /// The directories the confined shell must not see: the product's configuration
    /// root, state root and the bundled Forgejo's data root.
    #[serde(default)]
    pub hide: Vec<PathBuf>,
}

/// The non-secret context that lets a run declare the Forgejo tools.
///
/// It is written beside the plan, before the process starts, and holds no token: the
/// URL, the agent's recorded login and the repositories cloned into its workspace.
/// The token is read by the run from the credentials file, at the moment a call is
/// made, exactly like the model key. A run with no context file declares `shell`
/// alone, so an old run record and a run with no Forgejo behave the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunForgejo {
    /// The configured instance, without a trailing slash.
    pub url: String,
    /// The agent's own Forgejo login, as the identity recorded it.
    pub login: String,
    /// The repositories cloned into this agent's workspace.
    pub repositories: Vec<RunForgejoRepository>,
}

/// One cloned repository a run may act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunForgejoRepository {
    /// `owner/name`.
    pub full_name: String,
    /// The directory directly below the workspace where the clone lives.
    pub directory: String,
}

impl RunForgejo {
    /// The directory one named repository lives in, when it is cloned here.
    pub fn repository(&self, full_name: &str) -> Option<&RunForgejoRepository> {
        self.repositories
            .iter()
            .find(|repository| repository.full_name == full_name)
    }

    /// Every cloned repository's `owner/name`, in the order they were recorded.
    pub fn repository_names(&self) -> Vec<String> {
        self.repositories
            .iter()
            .map(|repository| repository.full_name.clone())
            .collect()
    }
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
    /// The Forgejo tools this run may declare, when all three gates hold.
    pub forgejo: Option<RunForgejo>,
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

/// The marker a run leaves when its plan promised a boundary the run-time layout
/// cannot keep. The engine reads it and stops claiming confinement for the board
/// (B9-R-R); it holds one reason sentence, never a secret.
pub const CONFINEMENT_REFUSED_FILE: &str = "confinement-refused.txt";

/// The non-secret Forgejo context file, written before a run starts.
pub const FORGEJO_CONTEXT_FILE: &str = "forgejo.json";

/// Record a run-time confinement refusal, `0600` like every record.
pub fn write_confinement_refusal(run_dir: &Path, reason: &str) -> io::Result<()> {
    crate::mode::write_private(
        run_dir.join(CONFINEMENT_REFUSED_FILE),
        reason.trim().as_bytes(),
    )
}

/// Why a run's promised boundary was refused, when it was. A missing file is the
/// normal case: the plan's method was used.
pub fn read_confinement_refusal(run_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(run_dir.join(CONFINEMENT_REFUSED_FILE)).ok()?;
    let reason = text.trim();
    if reason.is_empty() {
        None
    } else {
        Some(reason.to_owned())
    }
}

/// The log file's name.
pub const LOG_FILE: &str = "run.log";

/// Write the run's Forgejo context, `0600` like every record.
pub fn write_forgejo_context(run_dir: &Path, context: &RunForgejo) -> io::Result<()> {
    let text = serde_json::to_string_pretty(context).map_err(io::Error::other)?;
    crate::mode::write_private(run_dir.join(FORGEJO_CONTEXT_FILE), text.as_bytes())
}

/// Read a run's Forgejo context, when one was written and is readable.
///
/// A missing or unreadable file means the run declares `shell` alone: the safe
/// direction is no tools, never a guessed repository list.
pub fn read_forgejo_context(run_dir: &Path) -> Option<RunForgejo> {
    let text = fs::read_to_string(run_dir.join(FORGEJO_CONTEXT_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

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
    /// The process exited cleanly, but its own final summary says it could not do the
    /// instruction. Exit 0 is not proof of success (F4).
    Refused,
    /// The process exited cleanly, did nothing, and its own summary says there was
    /// nothing assigned to do.
    NoWork,
    /// The process exited cleanly but at least one tool call failed (a non-zero exit
    /// or a signal) and was not itself a refused write. It is never a plain `✓`
    /// (correction 4, the mark rule).
    Warned,
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
    pub const ALL: [RunOutcome; 7] = [
        RunOutcome::Succeeded,
        RunOutcome::Refused,
        RunOutcome::NoWork,
        RunOutcome::Warned,
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
            RunOutcome::Refused => "refused",
            RunOutcome::NoWork => "no_work",
            RunOutcome::Warned => "warned",
            RunOutcome::Failed => "failed",
            RunOutcome::Stopped => "stopped",
            RunOutcome::TimedOut => "timed_out",
        }
    }

    /// Read the word back. An unknown word is `None`, never a guess.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "succeeded" => Some(RunOutcome::Succeeded),
            "refused" => Some(RunOutcome::Refused),
            "no_work" => Some(RunOutcome::NoWork),
            "warned" => Some(RunOutcome::Warned),
            "failed" => Some(RunOutcome::Failed),
            "stopped" => Some(RunOutcome::Stopped),
            "timed_out" => Some(RunOutcome::TimedOut),
            _ => None,
        }
    }

    /// Whether the run went the way it was meant to.
    ///
    /// C-3: `succeeded` is the only outcome that is not something to explain. A clean
    /// exit whose own summary refuses the instruction, or reports no work, is not one
    /// of them (F4).
    pub fn is_success(self) -> bool {
        matches!(self, RunOutcome::Succeeded)
    }
}

/// One write a run made in Forgejo, as the Runs note shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoAction {
    /// `commented on owner/repo#12`, `opened #3` or `updated #3`.
    pub sentence: String,
    /// The web URL of the comment or pull request.
    pub url: String,
}

/// One run's ending, as the engine reads it from the records on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    /// The run's identity.
    pub run_id: String,
    /// How it ended, when a terminal record is there.
    pub outcome: Option<RunOutcome>,
    /// Why it ended that way, when the record says.
    pub reason: Option<String>,
    /// The run's directory.
    pub dir: PathBuf,
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
        summaries.push(summarise(run_id, &run_dir));
    }
    summaries
}

/// Read one run's terminal record into a summary.
fn summarise(run_id: &str, run_dir: &Path) -> RunSummary {
    let (outcome, reason) = match transcript::terminal_record(run_dir) {
        Some(Record::RunFinished {
            outcome, reason, ..
        }) => (RunOutcome::parse(&outcome), reason),
        _ => (None, None),
    };
    RunSummary {
        run_id: run_id.to_owned(),
        outcome,
        reason,
        dir: run_dir.to_path_buf(),
    }
}

/// The writes one run made in Forgejo, oldest first, read from its transcript.
///
/// A read-only call leaves no action here; every call remains in the transcript. A
/// successful write carries the URL of what it changed, so the Runs note shows the
/// person the action and the page it is on. A record whose action or URL is missing is
/// not invented: the transcript still holds the call.
pub fn forgejo_actions(run_dir: &Path) -> Vec<ForgejoAction> {
    let Ok(file) = transcript::read_checked(run_dir) else {
        return Vec::new();
    };
    let Some(file) = file else {
        return Vec::new();
    };
    file.records
        .iter()
        .filter_map(|record| match record {
            Record::ForgejoResult {
                action: Some(action),
                repository,
                number,
                url: Some(url),
                ..
            } => {
                let number = number
                    .map(|number| format!("#{number}"))
                    .unwrap_or_default();
                let sentence = match action.as_str() {
                    "commented" => match repository {
                        Some(repository) => format!("commented on {repository}{number}"),
                        None => format!("commented{number}"),
                    },
                    "opened" => format!("opened {number}"),
                    "updated" => format!("updated {number}"),
                    other => format!("{other} {number}"),
                };
                Some(ForgejoAction {
                    sentence: sentence.trim_end().to_owned(),
                    url: url.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// The file the run writes while a tool call is in flight: the tool's name and the
/// unix time it started, two lines. The engine reads it so a card can say
/// `shell · 12s` instead of a stale line.
pub const CURRENT_TOOL_FILE: &str = "current-tool";

/// The file the run writes when a tool call fails: `tool: first clause`. The engine
/// reads it for the card's `✗` line; the run log has the same line.
pub const FAILURE_FILE: &str = "last-failure";

/// The tool call in flight, and when it started (from the file's own mtime).
pub fn read_current_tool(run_dir: &Path) -> Option<(String, std::time::SystemTime)> {
    let path = run_dir.join(CURRENT_TOOL_FILE);
    let started = fs::metadata(&path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())?;
    let text = fs::read_to_string(&path).ok()?;
    let tool = text.lines().next()?.trim();
    if tool.is_empty() {
        return None;
    }
    Some((tool.to_owned(), started))
}

/// The last failed tool call, as the card shows it: one line, no raw output.
pub fn read_failure(run_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(run_dir.join(FAILURE_FILE)).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

/// The work one run did, correctly typed, at most three short lines.
///
/// A call that only *reads* is not work: a later check run must not relabel the pull
/// request it read as an issue (W31-3). The rows are `issue #N` for the issue the run
/// read and did not open a pull request for, `pull request #N from <branch>` for a
/// request it opened or updated (with the branch its own push witness recorded), and
/// `comment on #N` for a comment it wrote. A run with no Forgejo write yields no
/// lines, so the previous work block can stay until a later run does new work.
pub fn work_summary(run_dir: &Path) -> Vec<String> {
    let Ok(file) = transcript::read_checked(run_dir) else {
        return Vec::new();
    };
    let Some(file) = file else {
        return Vec::new();
    };
    let mut read: Option<(String, u64)> = None;
    let mut pulls: Vec<u64> = Vec::new();
    let mut comments: Vec<u64> = Vec::new();
    for record in &file.records {
        match record {
            Record::ForgejoCall {
                tool,
                repository: Some(repository),
                number: Some(number),
                ..
            } if tool == crate::forgejo_tools::READ_ISSUE_OR_PULL => {
                read = Some((repository.clone(), *number));
            }
            Record::ForgejoResult {
                action: Some(action),
                number: Some(number),
                ..
            } => match action.as_str() {
                "opened" | "updated" => {
                    pulls.retain(|seen| seen != number);
                    pulls.push(*number);
                }
                "commented" => {
                    comments.retain(|seen| seen != number);
                    comments.push(*number);
                }
                _ => {}
            },
            _ => {}
        }
    }
    // A push with no comment or pull request is still work (review N5): the branch
    // is read before the early return, so the witness is never unreachable.
    let branch = read_push_witness(run_dir)
        .map(|witness| witness.branch)
        .filter(|branch| !branch.trim().is_empty());
    if pulls.is_empty() && comments.is_empty() && branch.is_none() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if let Some((_repository, number)) = read
        && !pulls.contains(&number)
    {
        lines.push(format!("issue #{number}"));
    }
    if let Some(number) = pulls.last() {
        match branch.as_deref().filter(|branch| !branch.trim().is_empty()) {
            Some(branch) => lines.push(format!("pull request #{number} from {branch}")),
            None => lines.push(format!("pull request #{number}")),
        }
    } else if let Some(branch) = branch.as_deref() {
        lines.push(format!("branch {branch}"));
    }
    if let Some(number) = comments.last() {
        lines.push(format!("comment on #{number}"));
    }
    lines.truncate(3);
    lines
}

/// The newest work block across an agent's runs, newest run first. The block stays
/// until a later run does new work; a read-only run leaves it unchanged.
pub fn latest_work(runs: &[RunSummary]) -> Vec<String> {
    runs.iter()
        .rev()
        .map(|summary| work_summary(&summary.dir))
        .find(|lines| !lines.is_empty())
        .unwrap_or_default()
}

/// The final summary of the newest run that did work, newest run first.
///
/// The card prefers it over a later check run's reason: a no-op must not erase the
/// one-line account of the work the person can still see in the work block (F-10).
pub fn latest_work_result(runs: &[RunSummary]) -> Option<String> {
    runs.iter()
        .rev()
        .find(|summary| !work_summary(&summary.dir).is_empty())
        .and_then(|summary| summary.reason.clone())
}

/// Whether one run did work, as its own transcript and push witness show.
pub fn run_did_work(run_dir: &Path) -> bool {
    !work_summary(run_dir).is_empty()
}

/// Whether the run's own log says no open issue was assigned to this agent.
///
/// The list tool's successful empty answer is the one fact the model is given; the
/// Agent screen turns it into the sentence that names the fix instead of leaving an
/// empty work block (F2).
pub fn says_no_assigned_work(run_dir: &Path, login: &str) -> bool {
    fs::read_to_string(run_dir.join(LOG_FILE))
        .map(|text| {
            text.contains("no open issue is assigned to")
                || text.contains(&format!("no open issue is assigned to `{login}`"))
        })
        .unwrap_or(false)
}

/// Read a run's push witness, when one was written.
pub fn read_push_witness(run_dir: &Path) -> Option<PushWitness> {
    let text = fs::read_to_string(run_dir.join(PUSH_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The file that records a branch the run actually pushed.
///
/// Written by the agent process from the repository's own state after the run, not
/// from any command's text: the remote-tracking ref must have moved to the local
/// branch's commit, and git's reflog for that ref must record the update as a push.
/// A run whose only command is `echo git push` leaves no witness and therefore offers
/// no pull-request step. A run recorded before this file existed has none either, so
/// an old record makes no claim rather than guessing.
pub const PUSH_FILE: &str = "push.json";

/// What a run's push witness says: the branch and the remote-tracking ref that moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushWitness {
    /// The local branch the pushed ref corresponds to.
    pub branch: String,
    /// The remote-tracking ref whose object changed during the run.
    pub remote_ref: String,
    /// `owner/name` of the repository the push moved, when the clone named one.
    ///
    /// A witness written before this field existed has `None`; the Runs note then
    /// makes no claim, because it cannot match a lookup by repository.
    #[serde(default)]
    pub repository: Option<String>,
    /// The base branch the push was measured against, when the clone names one.
    #[serde(default)]
    pub base: Option<String>,
}

/// Write the push witness into the run's directory, `0600` like every record.
pub fn write_push_witness(run_dir: &Path, branch: &str, remote_ref: &str) -> io::Result<()> {
    write_push_witness_at(run_dir, branch, remote_ref, None, None)
}

/// Write the witness with the repository and base the run's own clone named.
pub fn write_push_witness_at(
    run_dir: &Path,
    branch: &str,
    remote_ref: &str,
    repository: Option<&str>,
    base: Option<&str>,
) -> io::Result<()> {
    let witness = PushWitness {
        branch: branch.to_owned(),
        remote_ref: remote_ref.to_owned(),
        repository: repository.map(str::to_owned),
        base: base.map(str::to_owned),
    };
    let text = serde_json::to_string_pretty(&witness).map_err(io::Error::other)?;
    crate::mode::write_private(run_dir.join(PUSH_FILE), text.as_bytes())
}

/// Whether a process with this id is still alive.
///
/// A process owned by another user is still alive; only a process that is gone counts
/// as dead. The editor's stale-scratch sweep and the engine's shutdown check use it.
pub fn process_alive(pid: u32) -> bool {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;

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
            confinement: "none".to_owned(),
            hide: Vec::new(),
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
        for outcome in RunOutcome::ALL {
            assert_eq!(RunOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(RunOutcome::parse("half-done"), None);
        assert!(!RunOutcome::Refused.is_success());
        assert!(!RunOutcome::NoWork.is_success());
        assert!(!RunOutcome::Warned.is_success());
    }

    #[test]
    fn the_work_block_types_a_pull_request_and_keeps_the_last_work() {
        let root = scratch_root("work-summary");
        let idle = root.join("run-1700000000001-0001");
        let work = root.join("run-1700000000002-0002");
        fs::create_dir_all(&idle).expect("idle run");
        fs::create_dir_all(&work).expect("work run");

        // A later check run reads a pull request and does nothing else. That read is
        // not work and must not relabel the request as an issue (W31-3).
        let mut transcript = transcript::Transcript::open(&idle).expect("transcript");
        transcript
            .append(&Record::forgejo_call(
                1,
                1,
                crate::forgejo_tools::READ_ISSUE_OR_PULL,
                Some("team/website"),
                Some(5),
                "no body",
            ))
            .expect("call");
        transcript
            .append(&Record::forgejo_result(
                1,
                1,
                crate::forgejo_tools::READ_ISSUE_OR_PULL,
                Some("team/website"),
                Some(5),
                "ok",
                None,
                None,
            ))
            .expect("result");
        assert!(work_summary(&idle).is_empty(), "a read is not work");

        // A work run: it read issue #1, opened pull request #5 from fix/api-link and
        // commented on #1. The branch comes from the run's own push witness.
        let mut transcript = transcript::Transcript::open(&work).expect("transcript");
        transcript
            .append(&Record::forgejo_call(
                1,
                1,
                crate::forgejo_tools::READ_ISSUE_OR_PULL,
                Some("team/website"),
                Some(1),
                "no body",
            ))
            .expect("read issue");
        transcript
            .append(&Record::forgejo_result(
                1,
                2,
                crate::forgejo_tools::OPEN_PULL_REQUEST,
                Some("team/website"),
                Some(5),
                "ok",
                Some("http://127.0.0.1:3000/team/website/pulls/5"),
                Some("opened"),
            ))
            .expect("open");
        transcript
            .append(&Record::forgejo_result(
                1,
                3,
                crate::forgejo_tools::COMMENT,
                Some("team/website"),
                Some(1),
                "ok",
                Some("http://127.0.0.1:3000/team/website/issues/1#comment"),
                Some("commented"),
            ))
            .expect("comment");
        write_push_witness_at(
            &work,
            "fix/api-link",
            "refs/remotes/origin/fix/api-link",
            None,
            None,
        )
        .expect("witness");
        let lines = work_summary(&work);
        assert_eq!(
            lines,
            vec![
                "issue #1".to_owned(),
                "pull request #5 from fix/api-link".to_owned(),
                "comment on #1".to_owned()
            ]
        );
        assert!(
            !lines.iter().any(|line| line == "issue #5"),
            "the pull request was labelled an issue: {lines:?}"
        );

        let runs = vec![
            RunSummary {
                run_id: "run-1700000000001-0001".to_owned(),
                outcome: Some(RunOutcome::Succeeded),
                reason: None,
                dir: idle.clone(),
            },
            RunSummary {
                run_id: "run-1700000000002-0002".to_owned(),
                outcome: Some(RunOutcome::Succeeded),
                reason: None,
                dir: work.clone(),
            },
        ];
        assert_eq!(latest_work(&runs), lines, "the latest work block was lost");
    }

    #[test]
    fn a_push_with_no_comment_or_pull_request_is_work() {
        // Review N5: the early return used to sit before the push witness, so a run
        // that only pushed a branch left no work row at all.
        let root = scratch_root("work-branch-only");
        let run = root.join("run-1700000000003-0001");
        fs::create_dir_all(&run).expect("a run directory");
        // A real run always has a transcript; an empty one is enough for the work
        // reader to get as far as the push witness.
        transcript::Transcript::open(&run).expect("the transcript");
        write_push_witness_at(
            &run,
            "fix/only-push",
            "refs/remotes/origin/fix/only-push",
            Some("team/website"),
            Some("main"),
        )
        .expect("the witness");
        assert_eq!(work_summary(&run), vec!["branch fix/only-push".to_owned()]);

        let empty = root.join("run-1700000000003-0002");
        fs::create_dir_all(&empty).expect("an empty run");
        assert!(
            work_summary(&empty).is_empty(),
            "no write and no push is not work"
        );
    }
}
