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
//!     first-ending.json the write-once fact of which ending happened first
//!     skills/           the skill set this run had installed
//! ```

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::prelude::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::engine::session::RunningSession;

static ATOMIC_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

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

/// The native program's immutable run facts, written before the run starts.
pub const NATIVE_PLAN_FILE: &str = "native.json";

/// The native child's positive process identity, written as soon as it exists.
pub const NATIVE_IDENTITY_FILE: &str = "native.identity";

/// The non-secret facts of one native run. The program, version, connection,
/// model and explicit Codex sandbox are frozen here; a later settings save cannot
/// change what an active or finished run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlan {
    /// The program id (`pi`, `codex`, `dsh`, `opencode`).
    pub harness: String,
    /// The actual program version.
    pub version: String,
    /// The executable.
    pub program: PathBuf,
    /// The program's product-owned install root (read/exec handled separately).
    pub install_root: PathBuf,
    /// `chat-completions`, `responses` or `anthropic-messages`.
    pub protocol: String,
    /// API base URL.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// `native` for the program's own sandbox, `full-access` for the explicit
    /// Codex compatibility choice inside the product Landlock.
    #[serde(default)]
    pub codex_sandbox: String,
}

impl NativePlan {
    /// Write the plan at 0600.
    pub fn write(&self, run_dir: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        crate::mode::write_private(run_dir.join(NATIVE_PLAN_FILE), text.as_bytes())
    }

    /// Read the plan, or `None` for a built-in run.
    pub fn read(run_dir: &Path) -> io::Result<Option<Self>> {
        match fs::read_to_string(run_dir.join(NATIVE_PLAN_FILE)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
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
/// Which ending happened first for a run: its own terminal record, or the first
/// engine stop/deadline/shutdown request. It is recorded once, when the first ending
/// occurs, and is never re-sampled by a later Stop, shutdown, settle or reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstEnding {
    /// The run wrote its own terminal record before any engine stop was requested.
    Terminal,
    /// An engine stop, deadline or shutdown was requested while no terminal existed.
    Stop,
}

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
    // Each request keeps the branch its own tool result read, so several pushes and
    // pull requests in one run do not borrow the first (or newest) push's branch.
    let mut pulls: Vec<(u64, Option<String>)> = Vec::new();
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
                branch,
                ..
            } => match action.as_str() {
                "opened" | "updated" => {
                    pulls.retain(|(seen, _)| seen != number);
                    pulls.push((
                        *number,
                        branch.clone().filter(|branch| !branch.trim().is_empty()),
                    ));
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
        && !pulls.iter().any(|(seen, _)| *seen == number)
    {
        lines.push(format!("issue #{number}"));
    }
    if let Some((number, pull_branch)) = pulls.last() {
        // The request's own result is the only witness for its branch. An older
        // record that did not carry one makes no branch claim: pairing it with the
        // run's single push witness is exactly the wrong-branch bug for a multi-PR
        // run, so there is no fallback to guess.
        let shown = pull_branch
            .as_deref()
            .filter(|branch| !branch.trim().is_empty());
        match shown {
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
    use crate::test_wait::wait_until;

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
        // commented on #1. The branch comes from that request's own tool result.
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
            .append(&Record::forgejo_result_with_branch(
                1,
                2,
                crate::forgejo_tools::OPEN_PULL_REQUEST,
                Some("team/website"),
                Some(5),
                "ok",
                Some("http://127.0.0.1:3000/team/website/pulls/5"),
                Some("opened"),
                Some("fix/api-link"),
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
    fn each_pull_request_keeps_the_branch_its_own_result_named() {
        let root = scratch_root("work-pr-branches");
        let run = root.join("run-1700000000004-0001");
        fs::create_dir_all(&run).expect("a run directory");
        let mut transcript = transcript::Transcript::open(&run).expect("transcript");
        transcript
            .append(&Record::forgejo_result_with_branch(
                1,
                1,
                crate::forgejo_tools::OPEN_PULL_REQUEST,
                Some("team/website"),
                Some(3),
                "ok",
                Some("http://127.0.0.1:3000/team/website/pulls/3"),
                Some("opened"),
                Some("fix/readme-receive"),
            ))
            .expect("the first request");
        transcript
            .append(&Record::forgejo_result_with_branch(
                1,
                2,
                crate::forgejo_tools::OPEN_PULL_REQUEST,
                Some("team/website"),
                Some(4),
                "ok",
                Some("http://127.0.0.1:3000/team/website/pulls/4"),
                Some("opened"),
                Some("fix/readme-separate"),
            ))
            .expect("the second request");
        // The run's push witness is a different, earlier push. The branch must come
        // from request #4's own result, never this file (the reported F4).
        write_push_witness_at(
            &run,
            "fix/readme-receive",
            "refs/remotes/origin/fix/readme-receive",
            Some("team/website"),
            Some("main"),
        )
        .expect("the witness");
        assert_eq!(
            work_summary(&run),
            vec!["pull request #4 from fix/readme-separate".to_owned()]
        );

        // An older record without a branch result omits an unknown branch, and makes
        // no branch claim when neither fact exists.
        let legacy = root.join("run-1700000000004-0002");
        fs::create_dir_all(&legacy).expect("a legacy run");
        let mut transcript = transcript::Transcript::open(&legacy).expect("transcript");
        transcript
            .append(&Record::forgejo_result(
                1,
                1,
                crate::forgejo_tools::OPEN_PULL_REQUEST,
                Some("team/website"),
                Some(5),
                "ok",
                Some("http://127.0.0.1:3000/team/website/pulls/5"),
                Some("opened"),
            ))
            .expect("the legacy request");
        assert_eq!(
            work_summary(&legacy),
            vec!["pull request #5".to_owned()],
            "a record with no branch made one up"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_legacy_multi_pull_request_record_does_not_borrow_the_push_witness() {
        let root = scratch_root("work-pr-legacy");
        let run = root.join("run-1700000000004-0003");
        fs::create_dir_all(&run).expect("a run directory");
        let mut transcript = transcript::Transcript::open(&run).expect("transcript");
        for number in [3, 4] {
            transcript
                .append(&Record::forgejo_result(
                    1,
                    number as u32,
                    crate::forgejo_tools::OPEN_PULL_REQUEST,
                    Some("team/website"),
                    Some(number),
                    "ok",
                    Some("http://127.0.0.1:3000/team/website/pulls/x"),
                    Some("opened"),
                ))
                .expect("the legacy request");
        }
        write_push_witness_at(
            &run,
            "fix/first-push",
            "refs/remotes/origin/fix/first-push",
            Some("team/website"),
            Some("main"),
        )
        .expect("the witness");
        assert_eq!(
            work_summary(&run),
            vec!["pull request #4".to_owned()],
            "a legacy multi-PR record borrowed the single push witness"
        );
        let _ = fs::remove_dir_all(&root);
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

    /// A child process used by the descriptor-exit tests. It holds either the
    /// inherited memfd capability or a named file descriptor, optionally makes its
    /// fd table inaccessible, announces readiness and then exits after a short hold.
    #[test]
    fn capability_descriptor_helper() {
        if std::env::var_os("GEWU_RUN_CAPABILITY_HELPER").is_none() {
            return;
        }
        let marker = std::env::var("GEWU_RUN_CAPABILITY_MARKER").expect("marker path");
        let hold_ms: u64 = std::env::var("GEWU_RUN_CAPABILITY_HOLD_MS")
            .expect("hold")
            .parse()
            .expect("hold number");
        let file = std::env::var_os("GEWU_RUN_CAPABILITY_FILE")
            .map(|path| fs::File::open(path).expect("the helper opens its descriptor"));
        if std::env::var_os("GEWU_RUN_CAPABILITY_NONDUMPABLE").is_some() {
            let _ = rustix::process::set_dumpable_behavior(
                rustix::process::DumpableBehavior::NotDumpable,
            );
        }
        fs::write(marker, b"ready").expect("the helper marker");
        std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        drop(file);
    }

    fn spawn_capability_helper(
        root: &Path,
        non_dumpable: bool,
        file: Option<&Path>,
        hold_ms: u64,
    ) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let marker = root.join("capability-helper-marker");
        let mut command = Command::new(std::env::current_exe().expect("the test executable"));
        command
            .args([
                "--exact",
                "run::tests::capability_descriptor_helper",
                "--nocapture",
            ])
            .env("GEWU_RUN_CAPABILITY_HELPER", "1")
            .env("GEWU_RUN_CAPABILITY_MARKER", &marker)
            .env("GEWU_RUN_CAPABILITY_HOLD_MS", hold_ms.to_string())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if non_dumpable {
            command.env("GEWU_RUN_CAPABILITY_NONDUMPABLE", "1");
        }
        if let Some(file) = file {
            command.env("GEWU_RUN_CAPABILITY_FILE", file);
        }
        let child = command.spawn().expect("the capability helper starts");
        wait_until("the capability helper's ready marker", || marker.is_file());
        child
    }

    fn wait_for_inaccessible_fds(pid: u32) {
        wait_until("the helper's inaccessible fd table", || {
            fs::read_dir(format!("/proc/{pid}/fd")).is_err()
        });
    }

    fn memfd_capability() -> (fs::File, Capability) {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::fs::MetadataExt;
        let fd = nix::sys::memfd::memfd_create(
            "gewu-exiting-capability",
            nix::sys::memfd::MFdFlags::empty(),
        )
        .expect("memfd");
        let mut capability = fs::File::from(fd);
        let token = [0x6d_u8; 16];
        capability.write_all(&token).expect("token");
        capability.seek(SeekFrom::Start(0)).expect("seek");
        let metadata = capability.metadata().expect("metadata");
        (
            capability,
            Capability {
                device: metadata.dev(),
                inode: metadata.ino(),
                token: hex(&token),
            },
        )
    }

    #[test]
    fn a_dying_descriptor_open_error_is_accepted_as_gone() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let root = scratch_root("capability-exit-open");
        let file = root.join("capability");
        fs::write(&file, [0x5b_u8; 16]).expect("token file");
        let metadata = fs::metadata(&file).expect("metadata");
        let capability = Capability {
            device: metadata.dev(),
            inode: metadata.ino(),
            token: hex(&[0x5b_u8; 16]),
        };
        let mut child = spawn_capability_helper(&root, false, Some(&file), 250);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).expect("deny open");
        let mut saw_error = false;
        for entry in fs::read_dir(format!("/proc/{}/fd", child.id())).expect("fd table") {
            let entry = entry.expect("fd");
            if fs::metadata(entry.path()).is_ok() && fs::File::open(entry.path()).is_err() {
                saw_error = true;
            }
        }
        assert!(
            saw_error,
            "the fixture did not produce a descriptor open error"
        );
        let result = pid_holds_capability(child.id(), &capability);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("restore");
        let _ = child.wait();
        let _ = fs::remove_dir_all(&root);
        assert!(
            !result.expect("the dying open-error holder is uncertain, not a holder"),
            "an exited open-error holder was reported as still holding the capability"
        );
    }

    #[test]
    fn a_persistently_inaccessible_capability_holder_stays_uncertain() {
        let root = scratch_root("capability-live-inaccessible");
        let (_capability_file, capability) = memfd_capability();
        let mut child = spawn_capability_helper(&root, true, None, 5_000);
        wait_for_inaccessible_fds(child.id());
        let started = std::time::Instant::now();
        let result = pid_holds_capability(child.id(), &capability);
        let elapsed = started.elapsed();
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.wait();
        let _ = fs::remove_dir_all(&root);
        let error = result.expect_err("a live inaccessible holder must stay uncertain");
        assert!(
            error.contains("file descriptors cannot be read"),
            "the live inaccessible holder error changed: {error}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(300),
            "the bounded recheck returned before its product window: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "the bounded recheck exceeded its 2 s test upper bound (the product rechecks for 500 ms): {elapsed:?}"
        );
    }
}

/// The file recording one run's process group, beside the pid record.
///
/// It is written as soon as the child exists. A reopened program reads it to stop
/// a group whose supervisor died before it could remove the record: the group id
/// outlives the leader as long as one command in the group is alive.
pub const PGID_FILE: &str = "session.pgid";

/// The shell's own pid, written by the run supervisor when its command's shell
/// starts. The shell can outlive a supervisor killed with `SIGKILL`; this record
/// is how a later start finds the group without `session.pid` (F-02/F-03).
pub const SHELL_PID_FILE: &str = "shell.pid";

/// The shell's process group, beside its pid.
pub const SHELL_PGID_FILE: &str = "shell.pgid";

/// The shell's live identity: its pid, its group, the kernel's boot id and the
/// process start time read from `/proc/<pid>/stat`. A stored number alone is not
/// identity after a reboot or pid reuse; this record is checked against the live
/// process before any signal.
pub const SHELL_IDENTITY_FILE: &str = "shell.identity";

/// The run's unnamed ownership capability: a record of the memfd's device,
/// inode and random token, plus the supervisor's inherited descriptor. Only a
/// process that inherited the descriptor can hold the object; the record alone
/// is not a capability and a named path cannot be opened into it.
pub const SHELL_CAPABILITY_FILE: &str = "shell.capability";

/// The marker written when a run's left-over group was stopped by a later start.
pub const STOPPED_FILE: &str = "run-stopped.json";
/// The write-once first-ending fact, written before any signal it governs.
pub const FIRST_ENDING_FILE: &str = "first-ending.json";
/// The evidence that the first-ending fact could not be established or honoured.
pub const ENDING_UNCERTAIN_FILE: &str = "ending-uncertain.json";

/// The native supervisor's latest failure-model state, beside the run's records.
///
/// It is written on every change and read by the engine when it has to end a run
/// from its own deadline: the last cause the harness emitted must survive the
/// child being killed, so a failing run is not recorded as only `timed_out`.
pub const NATIVE_PROGRESS_FILE: &str = "native-progress.json";

/// One native run's last emitted failure/progress cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeProgress {
    /// `running`, `waiting`, `retrying`, `failed` or `succeeded`.
    pub state: String,
    /// The last emitted cause, in the harness's own words.
    pub cause: String,
    /// Whether the cause came from a terminal harness event.
    pub terminal: bool,
    /// The first failed tool's diagnostic, kept as a warning. It is never a
    /// session-level or deadline failure cause.
    #[serde(default)]
    pub warning: Option<String>,
}

/// Record the native supervisor's latest cause for a deadline or stop ending.
pub fn write_native_progress(
    run_dir: &Path,
    state: &str,
    cause: &str,
    terminal: bool,
    warning: Option<&str>,
) -> io::Result<()> {
    let progress = NativeProgress {
        state: state.to_owned(),
        cause: cause.to_owned(),
        terminal,
        warning: warning.map(str::to_owned),
    };
    let text = serde_json::to_string_pretty(&progress).map_err(io::Error::other)?;
    crate::mode::write_private(run_dir.join(NATIVE_PROGRESS_FILE), text.as_bytes())
}

/// Read the native supervisor's latest cause, if it left one.
pub fn native_progress(run_dir: &Path) -> Option<NativeProgress> {
    let text = fs::read_to_string(run_dir.join(NATIVE_PROGRESS_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// One left-over process group's ending, written by the program that stopped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStopped {
    /// When the group was stopped, in RFC 3339 UTC.
    pub ts: String,
    /// The run the group belonged to.
    pub run_id: String,
    /// The recorded process-group id.
    pub pgid: u32,
    /// One plain sentence naming why it was stopped.
    pub reason: String,
}

/// The owner's run-process handle. The inner handle is private, so no module outside
/// `run.rs` can call its `signal` method; callers can only read pid/status.
pub struct RunHandle {
    inner: Box<dyn RunningSession>,
}

impl RunHandle {
    /// The child's pid / process-group id.
    pub fn pid(&self) -> u32 {
        self.inner.pid()
    }

    /// The exit status, when the child has ended.
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.inner.try_status()
    }

    fn signal(&mut self, force: bool) {
        self.inner.signal(force);
    }
}

/// Test-only terminal record construction. Integration tests are outside the owner
/// module; this helper exists only in debug builds and is not part of the release
/// surface.
#[cfg(debug_assertions)]
pub fn ensure_terminal_for_test(
    run_dir: &Path,
    outcome: RunOutcome,
    reason: Option<String>,
) -> Result<(), String> {
    ensure_terminal_record(run_dir, outcome, reason)
}

#[cfg(debug_assertions)]
pub fn terminal_record_for_test(
    run_id: &str,
    outcome: RunOutcome,
    exit_code: Option<i32>,
    duration_ms: u64,
    reason: Option<String>,
) -> Record {
    Record::run_finished(run_id, outcome, exit_code, duration_ms, reason)
}

/// The durable first-ending fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FirstEndingRecord {
    /// When the fact was recorded, in RFC 3339 UTC.
    ts: String,
    /// The run the fact belongs to.
    run_id: String,
    /// Which ending happened first.
    ending: FirstEnding,
}

/// The evidence that a run's ending is uncertain: the fact could not be written,
/// read, matched to this run, or honoured against its records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndingUncertainty {
    /// When the uncertainty was recorded, in RFC 3339 UTC.
    ts: String,
    /// The run the uncertainty belongs to.
    run_id: String,
    /// One plain sentence naming the evidence.
    error: String,
}

/// Install one private file without ever replacing an existing one. The caller's
/// first evidence wins; a racing second writer sees `AlreadyExists` and succeeds
/// without touching the file.
fn write_once_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let sequence = ATOMIC_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = parent.join(format!(
        ".write-once.part-{}-{}-{sequence}",
        std::process::id(),
        now_unix_ms()
    ));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(&stage)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&stage, path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let dir = fs::File::open(parent)?;
        dir.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&stage);
    result
}

/// Record that this run's ending cannot be decided cleanly.
///
/// This is written before a cleanup stop when the first-ending fact cannot be
/// written, and when a persisted fact contradicts the run's terminal record. A
/// reader that sees it must report uncertainty and keep the handles, never claim a
/// clean stop or outcome.
fn record_ending_uncertainty(run_dir: &Path, run_id: &str, error: &str) -> io::Result<()> {
    let expected = run_id_of_io(run_dir)?;
    if run_id != expected {
        return Err(io::Error::other(format!(
            "the uncertainty names run `{run_id}` but the directory is `{expected}`"
        )));
    }
    // The first evidence is the truth; a later reader must not replace it with a
    // narrower or derivative error. The no-clobber link below does the check
    // atomically, so two writers cannot both install evidence.
    let record = EndingUncertainty {
        ts: crate::time::rfc3339(now_unix_ms()),
        run_id: run_id.to_owned(),
        error: error.to_owned(),
    };
    let text = serde_json::to_string_pretty(&record).map_err(io::Error::other)?;
    write_once_private(&run_dir.join(ENDING_UNCERTAIN_FILE), text.as_bytes())
}

/// Read this run's uncertainty evidence, if any. A foreign or malformed record is
/// itself uncertainty, never "none".
fn ending_uncertainty(run_dir: &Path) -> Result<Option<String>, String> {
    let expected = run_id_of(run_dir)?;
    let path = run_dir.join(ENDING_UNCERTAIN_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "the run's ending uncertainty cannot be read: {error}"
            ));
        }
    };
    let record: EndingUncertainty = serde_json::from_str(&text)
        .map_err(|error| format!("the run's ending uncertainty is malformed: {error}"))?;
    if record.run_id != expected {
        return Err(format!(
            "the ending uncertainty belongs to run `{}`, not `{expected}`",
            record.run_id
        ));
    }
    Ok(Some(record.error))
}

fn run_id_of_io(run_dir: &Path) -> io::Result<&str> {
    run_dir
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| io::Error::other("the run directory has no run id"))
}

/// Read the durable first-ending fact, if this run has one.
///
/// A malformed record is an error, never "no fact": guessing would let a later
/// writer change which ending happened first.
pub(crate) fn recorded_first_ending(run_dir: &Path) -> Result<Option<FirstEnding>, String> {
    let expected = run_id_of(run_dir)?;
    if let Some(error) = ending_uncertainty(run_dir)? {
        return Err(format!("the run's ending is uncertain: {error}"));
    }
    let path = run_dir.join(FIRST_ENDING_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("the run's first ending cannot be read: {error}"));
        }
    };
    let record: FirstEndingRecord = serde_json::from_str(&text)
        .map_err(|error| format!("the run's first ending is malformed: {error}"))?;
    if record.run_id != expected {
        return Err(format!(
            "the first ending belongs to run `{}`, not `{expected}`; the ending is uncertain",
            record.run_id
        ));
    }
    Ok(Some(record.ending))
}

/// Record the first-ending fact once and return the fact that won.
///
/// The record is written to a private temporary file, fsynced, and installed with
/// an atomic no-clobber hard link. If two writers race, the first link wins and both
/// callers read the winner. This is called before any signal the fact governs, so a
/// crash/reopen cannot lose the first stop.
fn ensure_first_ending(
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
) -> Result<FirstEnding, String> {
    let expected = run_id_of(run_dir)?;
    if run_id != expected {
        return Err(format!(
            "the first ending names run `{run_id}` but the directory is `{expected}`; the ending is uncertain"
        ));
    }
    let existing = match recorded_first_ending(run_dir) {
        Ok(existing) => existing,
        Err(error) => {
            let _ = record_ending_uncertainty(run_dir, run_id, &error);
            return Err(error);
        }
    };
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let record = FirstEndingRecord {
        ts: crate::time::rfc3339(now_unix_ms()),
        run_id: run_id.to_owned(),
        ending: proposed,
    };
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| format!("the run's first ending cannot be encoded: {error}"))?;
    let target = run_dir.join(FIRST_ENDING_FILE);
    static FIRST_ENDING_SEQ: AtomicU64 = AtomicU64::new(0);
    let sequence = FIRST_ENDING_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = run_dir.join(format!(
        ".first-ending.part-{}-{}-{sequence}",
        std::process::id(),
        now_unix_ms()
    ));
    let write = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(&stage)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&stage, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Make the directory entry durable too. A directory that cannot be opened
        // or synced is a failure, not an implied success.
        let dir = fs::File::open(run_dir)?;
        dir.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&stage);
    write.map_err(|error| format!("the run's first ending cannot be written: {error}"))?;
    recorded_first_ending(run_dir)?
        .ok_or_else(|| "the run's first ending was written but cannot be read".to_owned())
}

/// Write the `run_stopped` record and the run's terminal record.
///
/// Both are written before the caller may remove the run's identity handles, and
/// either failure is returned rather than swallowed. The terminal ending is written
/// last and committed to disk, so a later open that finds it also has the marker;
/// an interrupted pair is retried from the still-present handles.
fn write_run_stopped(run_dir: &Path, run_id: &str, pgid: u32, reason: &str) -> io::Result<()> {
    let fact = match recorded_first_ending(run_dir) {
        Ok(fact) => fact,
        Err(error) => {
            let _ = record_ending_uncertainty(run_dir, run_id, &error);
            return Err(io::Error::other(error));
        }
    };
    match fact {
        Some(FirstEnding::Stop) => {
            // A run that already wrote a stopped ending has the one outcome the
            // reopen is owed. Rewriting it changes its timestamp and replaces a
            // truthful reason with the recovery sentence, so a clean stop looks
            // as if it had only been closed at reopen. A terminal that is not
            // stopped was written by the child during the first stop's grace: the
            // stop still wins and replaces that child ending.
            let terminal =
                transcript::terminal_record_checked(run_dir).map_err(io::Error::other)?;
            if let Some(transcript::Record::RunFinished { outcome, .. }) = terminal.as_ref()
                && outcome.as_str() == RunOutcome::Stopped.as_str()
            {
                return Ok(());
            }
            write_stop_marker(run_dir, run_id, pgid, reason)?;
            write_terminal_replacing(
                run_dir,
                run_id,
                RunOutcome::Stopped,
                reason,
                &ending::seal(),
            )
            .map_err(io::Error::other)
        }
        Some(FirstEnding::Terminal) => {
            let terminal =
                transcript::terminal_record_checked(run_dir).map_err(io::Error::other)?;
            if terminal.is_none() {
                let reason = "the run recorded a terminal ending first, but that ending cannot be read; the ending is uncertain";
                let _ = record_ending_uncertainty(run_dir, run_id, reason);
                return Err(io::Error::other(reason));
            }
            Ok(())
        }
        None => {
            // Runs from before the durable fact keep the legacy recovery: record the
            // cleanup stop and preserve whatever terminal the run wrote.
            write_stop_marker(run_dir, run_id, pgid, reason)?;
            transcript::record_outcome_if_missing(
                run_dir,
                run_id,
                RunOutcome::Stopped,
                Some(reason.to_owned()),
                0,
                &ending::seal(),
            )
            .map(|_| ())
        }
    }
}

/// Write only the stop marker, not a terminal record. A stop that cannot yet be
/// verified still gets a truthful marker; the terminal record waits for cleanup.
fn write_stop_marker(run_dir: &Path, run_id: &str, pgid: u32, reason: &str) -> io::Result<()> {
    let record = RunStopped {
        ts: crate::time::rfc3339(now_unix_ms()),
        run_id: run_id.to_owned(),
        pgid,
        reason: reason.to_owned(),
    };
    let text = serde_json::to_string_pretty(&record).map_err(io::Error::other)?;
    crate::mode::write_private(run_dir.join(STOPPED_FILE), text.as_bytes())
}

/// The run id a run directory's name carries.
fn run_id_of(run_dir: &Path) -> Result<&str, String> {
    run_dir
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| "the run directory has no run id".to_owned())
}

/// Record a run's ending durably unless its own supervisor already wrote one.
///
/// This is the ordering boundary: call it only after the run's group has been shown
/// to have no live member, and before [`clear_process_records`]. A failure leaves
/// every handle in place so a later Start can retry instead of trusting an absent
/// record.
fn ensure_terminal_record(
    run_dir: &Path,
    outcome: RunOutcome,
    reason: Option<String>,
) -> Result<(), String> {
    let run_id = run_id_of(run_dir)?;
    transcript::record_outcome_if_missing(run_dir, run_id, outcome, reason, 0, &ending::seal())
        .map(|_| ())
        .map_err(|error| format!("the run's ending cannot be recorded: {error}"))
}

/// The run's own terminal ending, as a read-only value for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEnding {
    /// The typed outcome.
    pub outcome: RunOutcome,
    /// The reason sentence, when the run wrote one.
    pub reason: Option<String>,
}

/// Read the run's terminal ending without exposing record construction to callers.
pub fn terminal_ending(run_dir: &Path) -> Result<Option<TerminalEnding>, String> {
    let record = transcript::terminal_record_checked(run_dir)
        .map_err(|error| format!("the run's ending cannot be read: {error}"))?;
    let Some(transcript::Record::RunFinished {
        outcome, reason, ..
    }) = record
    else {
        return Ok(None);
    };
    Ok(Some(TerminalEnding {
        outcome: RunOutcome::parse(&outcome)
            .ok_or_else(|| format!("the run's ending `{outcome}` is unknown"))?,
        reason,
    }))
}

/// Whether this run has a terminal record, without turning an unreadable
/// transcript into "no ending".
pub fn terminal_recorded(run_dir: &Path) -> Result<bool, String> {
    transcript::terminal_record_checked(run_dir)
        .map(|record| record.is_some())
        .map_err(|error| format!("the run's ending cannot be read: {error}"))
}

/// Write exactly one terminal record, replacing any earlier terminal record.
///
/// The transcript is append-only for normal records, but a stop that won the race
/// against a child's just-written ending must not leave two terminal outcomes. The
/// rewrite removes only `run_finished` records, preserves every other record in
/// order, writes one new terminal ending, and installs the file atomically at
/// 0600. It is called only after the child has been stopped, so no writer races it.
fn write_terminal_replacing(
    run_dir: &Path,
    run_id: &str,
    outcome: RunOutcome,
    reason: &str,
    _seal: &ending::EndingSeal,
) -> Result<(), String> {
    let mut records = match transcript::read_checked(run_dir)
        .map_err(|error| format!("the run's ending cannot be read: {error}"))?
    {
        Some(file) => file.records,
        None => Vec::new(),
    };
    records.retain(|record| !matches!(record, Record::RunFinished { .. }));
    // A stop is still an elapsed run. The opening record is the only start time
    // the engine has after the supervisor is gone; never write `0` when it exists.
    let duration_ms = transcript::first_started_at(run_dir)
        .and_then(|stamp| crate::time::parse_rfc3339(&stamp))
        .map(|started| crate::time::now_unix_ms().saturating_sub(started))
        .unwrap_or(0);
    records.push(Record::run_finished(
        run_id,
        outcome,
        None,
        duration_ms,
        Some(reason.to_owned()),
    ));
    let mut text = String::new();
    for record in &records {
        text.push_str(
            &serde_json::to_string(record)
                .map_err(|error| format!("the run's ending cannot be recorded: {error}"))?,
        );
        text.push('\n');
    }
    crate::mode::write_private(run_dir.join(transcript::TRANSCRIPT_FILE), text.as_bytes())
        .map_err(|error| format!("the run's ending cannot be recorded: {error}"))
}

/// Prepare a first ending without signalling a handle. This is the owner request
/// used when the caller has already reached the force phase of its own protocol.
pub fn prepare_ending(
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
) -> Result<FirstEnding, String> {
    pin_first_ending(run_dir, run_id, proposed)
}

/// The owner's fact-pinning step: persist the first ending for this run, and on any
/// failure persist the uncertainty evidence and return the error.
fn pin_first_ending(
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
) -> Result<FirstEnding, String> {
    match ensure_first_ending(run_dir, run_id, proposed) {
        Ok(ending) => Ok(ending),
        Err(error) => {
            let _ = record_ending_uncertainty(run_dir, run_id, &error);
            Err(error)
        }
    }
}

/// Ask a run process to stop, but only after its first ending is durable.
///
/// This is the only place a live run handle is signalled politely. The caller owns
/// the handle's state; the owner owns the ordering and the signal.
pub fn request_stop(
    handle: &mut RunHandle,
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
) -> Result<FirstEnding, String> {
    let ending = pin_first_ending(run_dir, run_id, proposed)?;
    if ending == FirstEnding::Stop {
        // A Terminal first ending means the supervisor's own finish won. Do not
        // signal it in the small window before it appends run_finished; the stop
        // only changes the desired state while the bounded finish completes.
        handle.signal(false);
    }
    Ok(ending)
}

/// Force a run handle after its polite grace. Private: the public request is
/// `request_force_stop`, which also consults the fact.
fn force_handle(handle: &mut RunHandle) {
    handle.signal(true);
}

/// Request the force step after a stop has already been requested.
pub fn request_force_stop(
    handle: &mut RunHandle,
    run_dir: &Path,
    run_id: &str,
    _reason: &str,
) -> Result<(), String> {
    pin_first_ending(run_dir, run_id, FirstEnding::Stop)?;
    force_handle(handle);
    Ok(())
}

/// The owner's stop-and-reap sequence for a child that must be stopped even when the
/// fact could not be established. The uncertainty evidence is written first, then
/// the child is stopped politely and by force and waited for.
pub fn stop_uncertain(
    handle: &mut RunHandle,
    run_dir: &Path,
    run_id: &str,
    error: &str,
    grace: Duration,
) -> String {
    let reason = format!("the run's first ending is uncertain: {error}");
    let reason = match record_ending_uncertainty(run_dir, run_id, &reason) {
        Ok(()) => reason,
        Err(marker) => format!("{reason}; the uncertainty evidence could not be written: {marker}"),
    };
    stop_and_wait(handle, grace);
    reason
}

/// Create the prepared directory at 0700; the private module's Owner is the only
/// constructor exposed to callers.
fn prepare_started_dir(run_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(run_dir)
        .map_err(|error| format!("{} cannot be created: {error}", run_dir.display()))?;
    crate::mode::set_mode(run_dir, crate::mode::PRIVATE_DIR_MODE)
        .map_err(|error| format!("{} cannot be restricted: {error}", run_dir.display()))
}

pub mod ending;
pub use ending::{Owner, PreparedRun, StartedRun};

/// The old PreparedRun block is replaced by `ending::PreparedRun`.
/// Cancel a run that exists only as a prepared directory, before any child exists.
/// This is the startup-cancellation owner path: it writes exactly one stopped
/// terminal and stop record, or explicit uncertainty, with no process handle.
pub fn request_cancel_prepared(run_dir: &Path, run_id: &str, reason: &str) -> Result<(), String> {
    if !run_dir.is_dir() {
        return Ok(());
    }
    let ending = pin_first_ending(run_dir, run_id, FirstEnding::Stop)?;
    if ending == FirstEnding::Terminal {
        if terminal_recorded(run_dir)? {
            return Ok(());
        }
        let error = "the prepared run's first ending is terminal, but its terminal record is missing; the ending is uncertain";
        let _ = record_ending_uncertainty(run_dir, run_id, error);
        return Err(error.to_owned());
    }
    let sentence = format!("{reason}; no child was started");
    write_stop_marker(run_dir, run_id, 0, &sentence)
        .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
    write_terminal_replacing(
        run_dir,
        run_id,
        RunOutcome::Stopped,
        &sentence,
        &ending::seal(),
    )
    .map_err(|error| format!("the run's ending cannot be recorded: {error}"))
}

/// Request a stop, then wait for the handle to end before it is dropped.
pub fn request_stop_and_wait(
    handle: &mut RunHandle,
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
    _reason: &str,
    grace: Duration,
) -> Result<(), String> {
    let ending = pin_first_ending(run_dir, run_id, proposed)?;
    if ending == FirstEnding::Stop {
        stop_and_wait(handle, grace);
    } else {
        wait_for_finish_or_force(handle, grace);
    }
    Ok(())
}

/// Pin the one ending, stop the owned child through its handle, and wait for its
/// status within `grace` (forcing after it) before the caller judges the process
/// group. A fact failure still sends no signal.
pub fn request_stop_and_wait_ending(
    handle: &mut RunHandle,
    run_dir: &Path,
    run_id: &str,
    proposed: FirstEnding,
    grace: Duration,
) -> Result<FirstEnding, String> {
    let ending = request_stop(handle, run_dir, run_id, proposed)?;
    if ending == FirstEnding::Stop {
        stop_and_wait(handle, grace);
    } else {
        wait_for_finish_or_force(handle, grace);
    }
    Ok(ending)
}

/// Stop one session handle politely, then by force, and wait for its status so the
/// child is reaped before the handle is dropped.
fn stop_and_wait(handle: &mut RunHandle, grace: Duration) {
    if handle.try_status().is_some() {
        return;
    }
    handle.signal(false);
    if wait_session_status(handle, grace) {
        return;
    }
    handle.signal(true);
    let _ = wait_session_status(handle, Duration::from_secs(1));
}

/// Give a supervisor whose own ending already won the bounded stop window, then
/// force only if it is genuinely stuck. No polite signal is sent in this path.
fn wait_for_finish_or_force(handle: &mut RunHandle, grace: Duration) {
    if handle.try_status().is_some() {
        return;
    }
    if wait_session_status(handle, grace) {
        return;
    }
    handle.signal(true);
    let _ = wait_session_status(handle, Duration::from_secs(1));
}

fn wait_session_status(handle: &mut RunHandle, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if handle.try_status().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The owner's raw process-group signal. Every run/child group signal uses this
/// function; adapters outside `run.rs` call it instead of `killpg`.
pub(crate) fn adapter_signal_run_group(pid: u32, force: bool) {
    let signal = if force {
        nix::sys::signal::Signal::SIGKILL
    } else {
        nix::sys::signal::Signal::SIGTERM
    };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// Request a signal to a starting child's group by the pid the job reported. The
/// job owns the wait and reaps it when it returns.
pub(crate) fn request_starting_stop(pid: u32, run_dir: Option<&Path>, force: bool, _reason: &str) {
    // The spawn job owns the handle, but a Stop or shutdown can arrive before that
    // job returns. Pin through the same first-ending arbitration as the handle
    // path: a Terminal winner is never signalled, and a Stop winner is durable
    // before the signal. A fact failure sends no signal; the job's handle settles.
    if let Some(run_dir) = run_dir {
        let Ok(run_id) = run_id_of(run_dir) else {
            return;
        };
        match pin_first_ending(run_dir, run_id, FirstEnding::Stop) {
            Ok(FirstEnding::Terminal) => return,
            Ok(FirstEnding::Stop) => {}
            Err(_) => return,
        }
    }
    adapter_signal_run_group(pid, force);
}

/// Settle a run whose self-written terminal record may already exist. The caller
/// supplies the durable first-ending fact; the owner performs the reap, the one
/// terminal write and the handle release.
pub fn request_finish(
    run_dir: &Path,
    pgid: u32,
    outcome: RunOutcome,
    reason: Option<String>,
    grace: Duration,
    stop: Option<String>,
    first_ending: FirstEnding,
) -> Result<bool, String> {
    settle_run_with_stop(run_dir, pgid, outcome, reason, grace, stop, first_ending)
}

/// Reopen recovery's only entry point.
pub fn recover_ending(run_dir: &Path, grace: Duration) -> Result<bool, String> {
    reap_leftover_run(run_dir, grace)
}

/// Write the one failed ending for a run that could not start a child.
pub fn request_failed_finish(run_dir: &Path, run_id: &str, reason: &str) -> Result<(), String> {
    // A Terminal fact written earlier must already have its terminal record. If it
    // does not, the records contradict each other: refuse to invent a Failed ending
    // and leave explicit uncertainty instead.
    if recorded_first_ending(run_dir)? == Some(FirstEnding::Terminal)
        && !terminal_recorded(run_dir)?
    {
        let uncertainty = "the run's first ending is terminal, but its terminal record is missing; the failed writer cannot invent an ending";
        let _ = record_ending_uncertainty(run_dir, run_id, uncertainty);
        return Err(uncertainty.to_owned());
    }
    if pin_first_ending(run_dir, run_id, FirstEnding::Terminal)? == FirstEnding::Stop {
        // The stop already owns the one ending. The failed spawn path must not add
        // a second terminal after the fact.
        return Ok(());
    }
    ensure_terminal_record(run_dir, RunOutcome::Failed, Some(reason.to_owned()))
}

/// Write the run process's own terminal ending and pin the Terminal fact first.
pub fn request_natural_finish(
    run_dir: &Path,
    run_id: &str,
    outcome: RunOutcome,
    exit_code: Option<i32>,
    duration_ms: u64,
    reason: Option<String>,
) -> Result<(), String> {
    let first = pin_first_ending(run_dir, run_id, FirstEnding::Terminal)?;
    if first == FirstEnding::Stop {
        // A stop won before this process reached its own ending. The stop settle
        // owns the terminal; do not append a natural one after it.
        return Ok(());
    }
    if transcript::terminal_record_checked(run_dir)
        .map_err(|error| format!("the run's ending cannot be read: {error}"))?
        .is_some()
    {
        // One terminal already exists; appending another would violate the owner's
        // one-ending rule.
        return Ok(());
    }
    let mut transcript = transcript::Transcript::open(run_dir)
        .map_err(|error| format!("the run's ending cannot be recorded: {error}"))?;
    transcript
        .append_terminal(
            &Record::run_finished(run_id, outcome, exit_code, duration_ms, reason),
            &ending::seal(),
        )
        .map_err(|error| format!("the run's ending cannot be recorded: {error}"))
}

/// Settle one finished run at the engine's boundary.
///
/// First the recorded group is stopped (or found already gone); then, and only
/// then, the run's ending is written durably; only after both have succeeded are the
/// identity handles removed. Any error keeps the handles and reaches the card, so
/// `Start` retries rather than running beside an unproven leftover.
///
/// A run the engine stopped (stopped, timed out, or failed at its deadline) also
/// gets a stop marker. The marker is written even when the reap cannot be verified,
/// with that uncertainty in its sentence; the terminal record waits for a verified
/// reap so a later retry can still decide it.
/// [`settle_run`] with the engine's own stop reason and the write-once first-ending
/// fact.
///
/// `FirstEnding::Terminal` means the run ended first: the existing terminal record is
/// its one outcome and no stop marker is written. `FirstEnding::Stop` means the
/// engine's stop was requested while no terminal existed; if the child managed to
/// write one during the grace window, the stop record replaces it so the run still
/// has exactly one terminal outcome. A later Stop, shutdown, settle, reap or reopen
/// cannot change this fact.
fn settle_run_with_stop(
    run_dir: &Path,
    pgid: u32,
    outcome: RunOutcome,
    reason: Option<String>,
    grace: std::time::Duration,
    stop: Option<String>,
    first_ending: FirstEnding,
) -> Result<bool, String> {
    let run_id = run_id_of(run_dir).map_err(categorize_cleanup)?;
    // The durable fact is the only source. If the caller has not persisted one yet,
    // write its proposed fact before the reap can signal the group. A later settle
    // reads the same fact even if the process that proposed it is gone.
    let first_ending = match recorded_first_ending(run_dir) {
        Ok(Some(persisted)) => persisted,
        Ok(None) => ensure_first_ending(run_dir, run_id, first_ending)?,
        Err(error) => {
            let _ = record_ending_uncertainty(run_dir, run_id, &error);
            return Err(error);
        }
    };
    let existing = transcript::terminal_record_checked(run_dir)
        .map_err(|error| format!("the run's ending cannot be read: {error}"))?;
    if stop.is_some() && first_ending == FirstEnding::Terminal && existing.is_none() {
        // The durable fact says the run ended first, but its terminal record is
        // missing or unreadable. The two records contradict each other; do not
        // reclassify the run as stopped. Keep the evidence and the handles.
        let reason = "the run's first ending is terminal, but its terminal record is missing; the ending is uncertain";
        let _ = record_ending_uncertainty(run_dir, run_id, reason);
        return Err(reason.to_owned());
    }
    if stop.is_some() && first_ending == FirstEnding::Terminal && existing.is_some() {
        // The run ended first. The Stop is only a desired-state change: keep the one
        // ending, write no stop marker, and still verify the group is gone.
        let reaped = reap(run_dir, pgid, grace)?;
        clear_process_records(run_dir);
        return Ok(reaped);
    }
    let reaped = reap(run_dir, pgid, grace);
    let mut stop_sentence = None;
    if let Some(stop_reason) = stop {
        let mut sentence = stop_reason;
        if let Err(error) = &reaped {
            sentence.push_str(&format!(
                "; its process group could not be verified as stopped: {}",
                categorize_cleanup(error.clone())
            ));
        }
        write_stop_marker(run_dir, run_id, pgid, &sentence)
            .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
        stop_sentence = Some(sentence);
    }
    match reaped {
        Ok(reaped) => {
            match stop_sentence {
                Some(sentence) => {
                    write_terminal_replacing(run_dir, run_id, outcome, &sentence, &ending::seal())
                        .map_err(categorize_cleanup)?;
                }
                None => {
                    ensure_terminal_record(run_dir, outcome, reason).map_err(categorize_cleanup)?;
                }
            }
            // `reap_process_group` only returns Ok when no unverified member survives,
            // and it writes its own run_stopped ending when it had to signal a group.
            clear_process_records(run_dir);
            Ok(reaped)
        }
        Err(error) => {
            // The stop happened, but its cleanup is unverified. Write the truthful
            // terminal ending anyway (with that uncertainty in the sentence) while
            // keeping every handle, so a later Start retries the cleanup and still
            // finds an outcome and reason.
            if let Some(sentence) = stop_sentence.as_deref() {
                write_terminal_replacing(run_dir, run_id, outcome, sentence, &ending::seal())
                    .map_err(categorize_cleanup)?;
            }
            Err(categorize_cleanup(error))
        }
    }
}

fn reap(run_dir: &Path, pgid: u32, grace: std::time::Duration) -> Result<bool, String> {
    let mut reaped = reap_process_group(pgid, run_dir, grace)?;
    // A native helper may have called setsid and therefore not be in the leader's
    // group. Its inherited capability is the ownership proof; enumerate and close
    // every such group before the identities are removed.
    reaped |= reap_capability_groups(run_dir, grace)?;
    Ok(reaped)
}

fn read_number(run_dir: &Path, name: &str) -> Option<u32> {
    fs::read_to_string(run_dir.join(name))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

/// One process's live identity: a boot id and the kernel's start time. The
/// combination survives pid reuse and reboot checks without reading any process
/// environment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessIdentity {
    boot: String,
    start: u64,
}

/// One shell record read back from a run directory.
#[derive(Debug, Clone)]
struct ShellRecord {
    pid: u32,
    pgid: u32,
    identity: ProcessIdentity,
}

/// The kernel boot id, read once. A process start time is only meaningful inside
/// one boot, so the pair is the identity. A read failure is an error, never a
/// silent "not this boot".
fn boot_id() -> io::Result<String> {
    static BOOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let value = BOOT.get_or_init(|| {
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    });
    value
        .clone()
        .ok_or_else(|| io::Error::other("the kernel boot id cannot be read"))
}

fn parse_stat_line(stat: &str) -> Option<(char, u32, u64)> {
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let group = fields.next()?.parse::<u32>().ok()?;
    // After state, ppid and pgrp come fields 6..=21; field 22 is starttime.
    for _ in 6..=21 {
        fields.next()?;
    }
    let start = fields.next()?.parse::<u64>().ok()?;
    Some((state, group, start))
}

/// State, process group and start time from `/proc/<pid>/stat`. A process that
/// has just gone is `Ok(None)`; a process whose stat cannot be read (permission,
/// malformed procfs) is an error, so uncertainty is never "nothing alive".
fn process_facts(pid: u32) -> io::Result<Option<(char, u32, u64)>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(io::Error::other(format!(
                "process {pid}'s stat cannot be read: {error}"
            )));
        }
    };
    Ok(parse_stat_line(&stat))
}

/// The live identity of `pid`, with its current process group. Zombies are not
/// live; they have already ended and only their parent's reaping is left.
fn process_identity(pid: u32) -> io::Result<Option<(u32, ProcessIdentity)>> {
    let Some((state, group, start)) = process_facts(pid)? else {
        return Ok(None);
    };
    if state == 'Z' {
        return Ok(None);
    }
    Ok(Some((
        group,
        ProcessIdentity {
            boot: boot_id()?,
            start,
        },
    )))
}

fn read_shell_record(run_dir: &Path) -> Option<ShellRecord> {
    let text = fs::read_to_string(run_dir.join(SHELL_IDENTITY_FILE)).ok()?;
    let mut fields = text.split_whitespace();
    let pid = fields.next()?.parse::<u32>().ok()?;
    let pgid = fields.next()?.parse::<u32>().ok()?;
    let boot = fields.next()?.to_owned();
    let start = fields.next()?.parse::<u64>().ok()?;
    Some(ShellRecord {
        pid,
        pgid,
        identity: ProcessIdentity { boot, start },
    })
}

/// Whether the shell record is still the same live process in the same group.
///
/// This is the only thing that makes a recorded group ours when the supervisor is
/// gone: the pid, boot and start time must all match the kernel's current answer.
fn shell_identity_is_live(run_dir: &Path, pgid: u32) -> io::Result<bool> {
    let Some(record) = read_shell_record(run_dir) else {
        return Ok(false);
    };
    if record.pgid != pgid {
        return Ok(false);
    }
    // A zombie shell still holds the group id and its pid cannot be reused
    // until it is reaped, so it can still prove the group is ours; the children
    // that survive under it are the live members `group_members` returns.
    let Some((_, live_group, start)) = process_facts(record.pid)? else {
        return Ok(false);
    };
    let boot = boot_id()?;
    Ok(live_group == pgid && ProcessIdentity { boot, start } == record.identity)
}

/// Record a command's shell so its group is found even after the supervisor and
/// its `session.*` records are gone. The supervisor writes this; the shell never
/// reads or changes it. The identity record is written first, so a crash at any
/// point leaves either a verifiable identity or no record that recovery would
/// trust.
pub fn write_shell_record(run_dir: &Path, pid: u32, _pgid: u32) -> io::Result<()> {
    let (live_group, identity) = process_identity(pid)?
        .ok_or_else(|| io::Error::other("the shell's live identity cannot be read from /proc"))?;
    let line = format!("{pid} {live_group} {} {}\n", identity.boot, identity.start);
    crate::mode::write_private(run_dir.join(SHELL_IDENTITY_FILE), line.as_bytes())?;
    crate::mode::write_private(run_dir.join(SHELL_PID_FILE), pid.to_string().as_bytes())?;
    crate::mode::write_private(
        run_dir.join(SHELL_PGID_FILE),
        live_group.to_string().as_bytes(),
    )
}

/// Record a native child's positive identity as soon as the child exists.
///
/// The native CLI is not the product's shell and may close inherited descriptors,
/// so its boot/start identity is the durable handle a later open uses after the
/// product run process is gone. The record is written by the run process before any
/// native output is consumed.
pub fn write_native_identity(run_dir: &Path, pid: u32) -> io::Result<()> {
    let (group, identity) = process_identity(pid)?
        .ok_or_else(|| io::Error::other("the native program's live identity cannot be read"))?;
    let line = format!("{pid} {group} {} {}\n", identity.boot, identity.start);
    crate::mode::write_private(run_dir.join(NATIVE_IDENTITY_FILE), line.as_bytes())
}

#[derive(Debug, Clone)]
struct NativeRecord {
    pid: u32,
    pgid: u32,
    identity: ProcessIdentity,
}

fn read_native_record(run_dir: &Path) -> Option<NativeRecord> {
    let text = fs::read_to_string(run_dir.join(NATIVE_IDENTITY_FILE)).ok()?;
    let mut fields = text.split_whitespace();
    Some(NativeRecord {
        pid: fields.next()?.parse().ok()?,
        pgid: fields.next()?.parse().ok()?,
        identity: ProcessIdentity {
            boot: fields.next()?.to_owned(),
            start: fields.next()?.parse().ok()?,
        },
    })
}

/// Whether the native identity names the same live process in the same group.
fn native_identity_is_live(run_dir: &Path, pgid: u32) -> io::Result<bool> {
    let Some(record) = read_native_record(run_dir) else {
        return Ok(false);
    };
    if record.pgid != pgid {
        return Ok(false);
    }
    let Some((_, live_group, start)) = process_facts(record.pid)? else {
        return Ok(false);
    };
    let boot = boot_id()?;
    Ok(live_group == pgid && ProcessIdentity { boot, start } == record.identity)
}

/// Create the run's anonymous ownership capability and record its object.
///
/// A memfd has no path an unrelated process can open; the supervisor holds the
/// descriptor before spawn, and the shell and every child inherit it. The record
/// gives recovery the object (device, inode) and a random token so an inode
/// reused by a later unrelated memfd is not mistaken for this run. The record
/// file itself is only a description; possessing it grants no capability.
pub fn prepare_shell_capability(run_dir: &Path) -> io::Result<fs::File> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let fd =
        nix::sys::memfd::memfd_create("gewu-run-capability", nix::sys::memfd::MFdFlags::empty())
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    let mut file = fs::File::from(fd);
    let token = random_token();
    file.write_all(&token)?;
    file.seek(SeekFrom::Start(0))?;
    let metadata = file.metadata()?;
    let record = format!("{} {} {}\n", metadata.dev(), metadata.ino(), hex(&token));
    crate::mode::write_private(run_dir.join(SHELL_CAPABILITY_FILE), record.as_bytes())?;
    Ok(file)
}

fn random_token() -> [u8; 16] {
    use std::io::Read;
    let mut token = [0_u8; 16];
    if let Ok(mut random) = fs::File::open("/dev/urandom")
        && random.read_exact(&mut token).is_ok()
    {
        return token;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128) << 64;
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = (seed >> ((index % 16) * 8)) as u8 ^ index as u8;
    }
    token
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone)]
struct Capability {
    device: u64,
    inode: u64,
    token: String,
}

fn read_capability(run_dir: &Path) -> Option<Capability> {
    let text = fs::read_to_string(run_dir.join(SHELL_CAPABILITY_FILE)).ok()?;
    let mut fields = text.split_whitespace();
    let device = fields.next()?.parse::<u64>().ok()?;
    let inode = fields.next()?.parse::<u64>().ok()?;
    let token = fields.next()?.to_owned();
    let _ = (device, inode);
    Some(Capability {
        device,
        inode,
        token,
    })
}

/// Whether one process holds the run's memfd capability. Permission/fd errors
/// are visible only for a process already known to be in the run's group.
///
/// A process that is exiting can transiently refuse its `/proc/<pid>/fd` reads
/// after it has already been signalled. Re-observe only that concrete descriptor
/// error class for a short bounded window. Absence is accepted only with positive
/// gone/zombie/exited facts; a live, unreadable or unknown process still returns
/// uncertainty and keeps its records.
fn pid_holds_capability(pid: u32, capability: &Capability) -> Result<bool, String> {
    const RECHECK: std::time::Duration = std::time::Duration::from_millis(500);
    const STEP: std::time::Duration = std::time::Duration::from_millis(10);
    let deadline = std::time::Instant::now() + RECHECK;
    loop {
        match pid_holds_capability_once(pid, capability) {
            Ok(found) => return Ok(found),
            Err((message, error)) if descriptor_error_may_be_exit(&error) => {
                // `/proc` absence is not proof of an ending by itself: a live
                // process can be hidden from procfs. Only a successful zero
                // signal is a positive "still exists" fact; a dead pid or a
                // zombie/dead state is a positive ending fact.
                if !process_alive(pid) {
                    return Ok(false);
                }
                match process_facts(pid) {
                    Ok(Some((state, _, _))) if process_state_has_ended(state) => return Ok(false),
                    Ok(Some(_)) | Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(message);
                        }
                        std::thread::sleep(STEP);
                    }
                    Err(stat) => {
                        return Err(format!(
                            "process {pid}'s state cannot be read while checking the run's capability: {stat}"
                        ));
                    }
                }
            }
            Err((message, _)) => return Err(message),
        }
    }
}

/// A positive ending fact from `/proc/<pid>/stat`. A stopped/traced process is
/// still live and unknown for recovery.
fn process_state_has_ended(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

/// Whether an `EINVAL` from `pidfd_open` is the already-gone case, not a live
/// process without an openable handle.
///
/// Linux answers `EINVAL` when the struct pid no longer has a task: the member
/// was reaped between `group_members()` and the open. A failed zero signal or a
/// gone/ended `/proc/<pid>/stat` is positive absence. A live, unreadable or live-
/// but-unverifiable process (for example a non-leader thread) keeps the
/// uncertainty, so the caller still reports it.
fn pidfd_einval_is_absence(pid: u32) -> bool {
    if !process_alive(pid) {
        return true;
    }
    match process_facts(pid) {
        Ok(None) => true,
        Ok(Some((state, _, _))) => process_state_has_ended(state),
        Err(_) => false,
    }
}

/// The descriptor-read error class a process can produce while it is exiting.
fn descriptor_error_may_be_exit(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
    ) || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
}

fn pid_holds_capability_once(
    pid: u32,
    capability: &Capability,
) -> Result<bool, (String, io::Error)> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let fds = match fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(fds) => fds,
        Err(error) => {
            return Err((
                format!(
                    "process {pid}'s file descriptors cannot be read while checking the run's capability: {error}"
                ),
                error,
            ));
        }
    };
    for fd in fds.flatten() {
        let target = match fs::metadata(fd.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err((
                    format!("process {pid}'s file descriptor cannot be inspected: {error}"),
                    error,
                ));
            }
        };
        if target.dev() != capability.device || target.ino() != capability.inode {
            continue;
        }
        let mut file = match fs::File::open(fd.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err((
                    format!("process {pid}'s capability cannot be opened: {error}"),
                    error,
                ));
            }
        };
        let mut token = [0_u8; 16];
        if file.read_exact(&mut token).is_ok() && hex(&token) == capability.token {
            return Ok(true);
        }
    }
    Ok(false)
}

fn capability_holders(pgid: u32, run_dir: &Path) -> Result<Vec<u32>, String> {
    let Some(capability) = read_capability(run_dir) else {
        return Ok(Vec::new());
    };
    let members = group_members(pgid).map_err(|error| error.to_string())?;
    let mut holders = Vec::new();
    for pid in members {
        if pid_holds_capability(pid, &capability)? {
            holders.push(pid);
        }
    }
    Ok(holders)
}

fn group_holds_capability(pgid: u32, run_dir: &Path) -> Result<bool, String> {
    Ok(!capability_holders(pgid, run_dir)?.is_empty())
}

/// Remove every process record for a run whose group was reaped (or was found
/// already gone), so a later reopen never acts on a stale group id.
pub fn clear_process_records(run_dir: &Path) {
    for name in [
        crate::engine::session::PID_FILE,
        PGID_FILE,
        SHELL_PID_FILE,
        SHELL_PGID_FILE,
        SHELL_IDENTITY_FILE,
        SHELL_CAPABILITY_FILE,
        NATIVE_IDENTITY_FILE,
    ] {
        let _ = fs::remove_file(run_dir.join(name));
    }
}

/// The recorded pid and process group of a run, when a record is readable.
///
/// The supervisor's own records are preferred; when it exited and removed them,
/// the shell record it wrote at spawn still names the group (F-02/F-03).
pub fn recorded_group(run_dir: &Path) -> Option<(u32, u32)> {
    if let Some(pid) = read_number(run_dir, crate::engine::session::PID_FILE) {
        return Some((pid, read_number(run_dir, PGID_FILE).unwrap_or(pid)));
    }
    if let Some(pgid) = read_number(run_dir, PGID_FILE) {
        return Some((pgid, pgid));
    }
    if let Some(pid) = read_number(run_dir, SHELL_PID_FILE) {
        return Some((pid, read_number(run_dir, SHELL_PGID_FILE).unwrap_or(pid)));
    }
    if let Some(pgid) = read_number(run_dir, SHELL_PGID_FILE) {
        return Some((pgid, pgid));
    }
    if let Some(record) = read_shell_record(run_dir) {
        return Some((record.pid, record.pgid));
    }
    read_native_record(run_dir).map(|record| (record.pid, record.pgid))
}

/// Whether a process of this run is still alive, believed through any durable
/// identity the run left behind.
///
/// `recorded_group` answers which group the stop path should try, and its first
/// record is the supervisor. A supervisor killed outright leaves that record
/// naming a dead process while the shell or the native child it started is still
/// alive in the same group; the shell record and `native.identity` still name
/// them. Checking only the first record would let the dead supervisor hide the
/// live child. Every candidate group, the supervisor's included, is believed
/// only through the same checks the stop path uses (`live_run_pids`), so a bare
/// pid, a reused number, a zombie or an unrelated live process is never called
/// this run's.
pub fn recorded_run_is_live(run_dir: &Path) -> bool {
    let mut groups = Vec::new();
    if let Some((_, pgid)) = recorded_group(run_dir) {
        groups.push(pgid);
    }
    if let Some(record) = read_shell_record(run_dir) {
        groups.push(record.pgid);
    }
    if let Some(record) = read_native_record(run_dir) {
        groups.push(record.pgid);
    }
    groups.sort_unstable();
    groups.dedup();
    for pgid in groups {
        if live_run_pids(pgid, run_dir).is_ok_and(|members| !members.is_empty()) {
            return true;
        }
    }
    // The inherited anonymous descriptor can outlive every pid record and still
    // be held by the run's group.
    capability_group(run_dir).is_ok_and(|group| group.is_some())
}

/// Every process in a recorded group, read from the kernel's `stat` only. No
/// process environment is read (F-02 review). An unreadable process table or
/// stat is an error, not an empty group (review-3b).
fn group_members(pgid: u32) -> io::Result<Vec<u32>> {
    let mut found = Vec::new();
    let entries = fs::read_dir("/proc")
        .map_err(|error| io::Error::other(format!("the process table cannot be read: {error}")))?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) =>
            {
                continue;
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "process {pid}'s stat cannot be read: {error}"
                )));
            }
        };
        let Some((state, group, _)) = parse_stat_line(&stat) else {
            continue;
        };
        // A zombie has already ended; only its parent's reaping is left. Counting
        // it as live would wait forever for a process that can no longer run.
        if state == 'Z' {
            continue;
        }
        if group == pgid {
            found.push(pid);
        }
    }
    Ok(found)
}

/// The live members of a run's recorded group, believed only through live
/// identity: the shell record still names the same live process in this group,
/// the inherited anonymous capability is held by a member, or the actual product
/// supervisor has the exact invocation for this run. A bare pgid or stored pid
/// number alone is never identity.
fn live_run_pids(pgid: u32, run_dir: &Path) -> Result<Vec<u32>, String> {
    let members = group_members(pgid).map_err(|error| error.to_string())?;
    if members.is_empty() {
        return Ok(members);
    }
    if shell_identity_is_live(run_dir, pgid).map_err(|error| error.to_string())?
        || group_holds_capability(pgid, run_dir)?
        || native_identity_is_live(run_dir, pgid).map_err(|error| error.to_string())?
    {
        return Ok(members);
    }
    // Before its first shell command, a supervisor has no shell capability yet.
    // Require both the executable and the complete private invocation; a path
    // retained by an unrelated directory reader is never enough.
    let supervisor = members.iter().any(|pid| {
        let Some(parts) = crate::bundled_forgejo::command_line(*pid) else {
            return false;
        };
        if parts.len() != 3 || parts[1] != "--run-agent" || Path::new(&parts[2]) != run_dir {
            return false;
        }
        let Ok(executable) = fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        let text = executable.to_string_lossy();
        let path = Path::new(text.strip_suffix(" (deleted)").unwrap_or(&text));
        path.file_name()
            .is_some_and(|name| name == "gewu-multiverse")
            || std::env::current_exe().is_ok_and(|ours| ours == path)
    });
    Ok(if supervisor { members } else { Vec::new() })
}

/// Members already observed while the group's live identity held. When the shell
/// dies after `SIGTERM`, its children stay in the group; their own boot/start
/// identity is remembered and rechecked individually, so escalation never signals
/// a reused pid.
type KnownMember = (u32, ProcessIdentity);

/// One process bound to a kernel pidfd. The handle names that exact process, not
/// a number that can be reused before the signal.
struct ReapTarget {
    pid: u32,
    pidfd: OwnedFd,
}

// A test can name one pid whose pidfd open lands after the member was reaped,
// inside the gap between `group_members()` and `open_pidfd()`. The test owns
// the child, so the seam can reap it for real before reporting the kernel's
// EINVAL for a struct pid with no task.
#[cfg(test)]
thread_local! {
    static PIDFD_REAP_SEAM: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct PidfdReapSeam;

#[cfg(test)]
impl PidfdReapSeam {
    fn arm(pid: u32) -> Self {
        PIDFD_REAP_SEAM.with(|seam| seam.set(Some(pid)));
        Self
    }
}

#[cfg(test)]
impl Drop for PidfdReapSeam {
    fn drop(&mut self) {
        PIDFD_REAP_SEAM.with(|seam| seam.set(None));
    }
}

#[cfg(test)]
fn take_reap_seam(pid: u32) -> bool {
    PIDFD_REAP_SEAM.with(|seam| {
        if seam.get() != Some(pid) {
            return false;
        }
        seam.set(None);
        true
    })
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    let handle = rustix::process::Pid::from_raw(pid as i32)
        .ok_or_else(|| io::Error::other("the process id is not positive"))?;
    #[cfg(test)]
    if take_reap_seam(pid) {
        // The member was live when `group_members()` listed it. Reap it now,
        // then answer as a kernel whose struct pid has outlived its task does:
        // EINVAL, not ESRCH.
        let raw = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::kill(raw, nix::sys::signal::Signal::SIGKILL);
        let _ = nix::sys::wait::waitpid(raw, None);
        return Err(io::Error::from_raw_os_error(
            nix::errno::Errno::EINVAL as i32,
        ));
    }
    rustix::process::pidfd_open(handle, rustix::process::PidfdFlags::empty())
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
}

fn pidfd_signal(target: &ReapTarget, signal: rustix::process::Signal) -> io::Result<()> {
    rustix::process::pidfd_send_signal(&target.pidfd, signal)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
}

fn rustix_signal(signal: nix::sys::signal::Signal) -> rustix::process::Signal {
    match signal {
        nix::sys::signal::Signal::SIGTERM => rustix::process::Signal::Term,
        nix::sys::signal::Signal::SIGKILL => rustix::process::Signal::Kill,
        _ => rustix::process::Signal::Term,
    }
}

/// Whether a run directory's process records can be read. A permission failure
/// must be visible, not look like "no records" (review-3).
fn process_records_readable(run_dir: &Path) -> Result<(), String> {
    if let Err(error) = fs::read_dir(run_dir) {
        return Err(format!("the run directory cannot be read: {error}"));
    }
    // read permission alone is not search permission; stat a name that should not
    // exist so EACCES cannot masquerade as an absent record.
    match fs::metadata(run_dir.join(".gewu-search-probe")) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("the run directory cannot be searched: {error}"));
        }
    }
    for name in [
        crate::engine::session::PID_FILE,
        PGID_FILE,
        SHELL_PID_FILE,
        SHELL_PGID_FILE,
        SHELL_IDENTITY_FILE,
        SHELL_CAPABILITY_FILE,
        NATIVE_IDENTITY_FILE,
        PLAN_FILE,
        crate::transcript::TRANSCRIPT_FILE,
    ] {
        let path = run_dir.join(name);
        match fs::metadata(&path) {
            Ok(_) => {
                if fs::read(&path).is_err() {
                    return Err(format!("the run's {name} record cannot be read"));
                }
            }
            // A truly absent record is not a failure. Permission/search failure
            // inside a readable directory is (review-3b).
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "the run's {name} record cannot be inspected: {error}"
                ));
            }
        }
    }
    Ok(())
}

/// The group of a live holder of the run's anonymous capability. This finds the
/// pre-record window and lost-record cases without any named object.
/// Whether an inaccessible same-user process could hold the run's capability.
///
/// The capability is inherited only through the run's own process tree, so a
/// process running as another user cannot have it. A same-user process whose
/// descriptor table cannot be read is the one case that must stay uncertain.
fn same_user_could_hold(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    // `/proc/self` is a magic link whose metadata is not this process's directory
    // owner in every namespace view; the status file's Uid line is explicit.
    let ours = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Uid:")
                    .and_then(|rest| rest.split_whitespace().nth(1))
                    .and_then(|uid| uid.parse::<u32>().ok())
            })
        });
    let theirs = fs::metadata(format!("/proc/{pid}")).map(|metadata| metadata.uid());
    matches!((ours, theirs), (Some(ours), Ok(theirs)) if ours == theirs)
}

/// Find a live group that holds the run's capability, or name the uncertainty.
///
/// A readable holder wins. If none is readable and any same-user process could not
/// be inspected, this returns an error rather than `None`: an ordinary supervisor
/// outcome is not proof that an original-group member is gone.
fn capability_group(run_dir: &Path) -> Result<Option<u32>, String> {
    let Some(capability) = read_capability(run_dir) else {
        return Ok(None);
    };
    let entries = fs::read_dir("/proc").map_err(|error| {
        format!("cleanup unverified: the process table cannot be read: {error}")
    })?;
    let mut uncertain: Option<String> = None;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        match pid_holds_capability(pid, &capability) {
            Ok(true) => {
                if let Some((_, group, _)) =
                    process_facts(pid).map_err(|error| error.to_string())?
                {
                    return Ok(Some(group));
                }
            }
            Ok(false) => {}
            Err(reason) => {
                // An inaccessible process of another user cannot hold this
                // user's inherited memfd. A same-user one must not be skipped:
                // it could be the run's surviving child.
                if same_user_could_hold(pid) {
                    uncertain = Some(reason);
                }
            }
        }
    }
    match uncertain {
        Some(reason) => Err(format!(
            "cleanup unverified: {reason}; a same-user process could still hold the run's capability"
        )),
        None => Ok(None),
    }
}

/// Observe the run's group, binding each candidate to a pidfd before its live
/// identity is checked. A pid is never signalled by number afterwards. While the
/// group has a live owner the observed identities are remembered; after the owner
/// is gone, only those exact processes are returned.
fn observe_run_group(
    pgid: u32,
    run_dir: &Path,
    known: &mut Vec<KnownMember>,
) -> (Vec<ReapTarget>, Option<String>) {
    let members = match group_members(pgid) {
        Ok(members) => members,
        Err(error) => {
            return (
                Vec::new(),
                Some(format!(
                    "the run's process group cannot be inspected: {error}"
                )),
            );
        }
    };
    if members.is_empty() {
        known.clear();
        return (Vec::new(), None);
    }
    let owned_before = match live_run_pids(pgid, run_dir) {
        Ok(members) => !members.is_empty(),
        Err(error) => return (Vec::new(), Some(error)),
    };
    let mut targets = Vec::new();
    let mut observed = Vec::new();
    let mut blocked = None;
    for pid in members {
        // Bind the kernel handle first: the identity read below is then about the
        // process the descriptor names, and a later reuse cannot retarget it.
        let pidfd = match open_pidfd(pid) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                let errno = error.raw_os_error();
                // ESRCH is the ordinary "already gone" answer. Linux also answers
                // EINVAL when the struct pid has no task left, which is the same
                // reaped-before-open race. Recheck before calling that a live
                // process without a handle; a live unverifiable pid keeps the
                // uncertainty below.
                if errno == Some(nix::errno::Errno::ESRCH as i32)
                    || (errno == Some(nix::errno::Errno::EINVAL as i32)
                        && pidfd_einval_is_absence(pid))
                {
                    continue;
                }
                blocked = Some(format!(
                    "unverified leftover: a live process has no kernel process handle, so it cannot be verified: {error}"
                ));
                continue;
            }
        };
        let identity = match process_identity(pid) {
            Ok(Some((live_group, identity))) if live_group == pgid => identity,
            Ok(_) => continue,
            Err(error) => {
                blocked = Some(format!(
                    "unverified leftover: a live process's identity cannot be verified: {error}"
                ));
                continue;
            }
        };
        observed.push((pid, identity));
        targets.push(ReapTarget { pid, pidfd });
    }

    let owned_now = match live_run_pids(pgid, run_dir) {
        Ok(members) => !members.is_empty(),
        Err(error) => return (Vec::new(), Some(error)),
    };
    let mut selected = Vec::new();
    if owned_before && owned_now {
        known.clear();
        known.extend(observed.iter().cloned());
        // The recorded shell is the owner's own process; only keep it while its
        // boot/start identity still matches. Children ride on the owner/capability
        // identity that made the group ours.
        let shell = read_shell_record(run_dir);
        for target in targets {
            if let Some(shell) = shell.as_ref()
                && target.pid == shell.pid
            {
                match shell_identity_is_live(run_dir, pgid) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        return (
                            Vec::new(),
                            Some(format!("the shell identity cannot be read: {error}")),
                        );
                    }
                }
            }
            selected.push(target);
        }
    } else {
        // No current owner: only the exact processes observed under the last live
        // owner, matched by pid and boot/start identity after the pidfd was opened.
        for target in targets {
            if let Some((pid, identity)) = observed.iter().find(|(pid, _)| *pid == target.pid)
                && known.iter().any(|(known_pid, known_identity)| {
                    known_pid == pid && known_identity == identity
                })
            {
                selected.push(target);
            }
        }
    }
    (selected, blocked)
}

fn signal_targets(targets: &[ReapTarget], signal: nix::sys::signal::Signal) -> Result<(), String> {
    for target in targets {
        match pidfd_signal(target, rustix_signal(signal)) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => {}
            Err(error) => {
                return Err(format!(
                    "live leftover: a run process (pid {}) cannot be signalled: {error}",
                    target.pid
                ));
            }
        }
    }
    Ok(())
}

/// Whether a run ever started: its transcript holds a record. A plan alone is a
/// prepared run, not a started one; an unreadable transcript is an error, never
/// "never started" (review-3b).
fn run_started(run_dir: &Path) -> Result<bool, String> {
    let file = match crate::transcript::read(run_dir) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "the run's transcript cannot be read, so its start cannot be verified: {error}"
            ));
        }
    };
    Ok(!file.records.is_empty() || file.partial.is_some())
}

/// The wall-clock boot time from `/proc/stat`'s `btime`.
fn boot_epoch() -> Option<u64> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// A run whose durable records were all created before this boot cannot have a
/// live process from that run; this is the bounded disposition for a stale,
/// recordless run. File mtimes, not the directory mtime, are used so touching the
/// directory neither extends nor creates a false block.
fn run_predates_boot(run_dir: &Path) -> bool {
    let Some(boot) = boot_epoch() else {
        return false;
    };
    let mut newest = 0_u64;
    let mut found = false;
    for name in [
        PLAN_FILE,
        crate::transcript::TRANSCRIPT_FILE,
        SHELL_IDENTITY_FILE,
        SHELL_CAPABILITY_FILE,
        NATIVE_IDENTITY_FILE,
    ] {
        let Ok(metadata) = fs::metadata(run_dir.join(name)) else {
            continue;
        };
        found = true;
        if let Ok(time) = metadata.modified()
            && let Ok(time) = time.duration_since(std::time::UNIX_EPOCH)
        {
            newest = newest.max(time.as_secs());
        }
    }
    if !found
        && let Ok(metadata) = fs::metadata(run_dir)
        && let Ok(time) = metadata.modified()
        && let Ok(time) = time.duration_since(std::time::UNIX_EPOCH)
    {
        newest = time.as_secs();
    }
    newest > 0 && newest < boot
}

/// Why a live group with no selected pidfd target is still not gone.
fn unverified_group(pgid: u32) -> Result<bool, String> {
    let members = group_members(pgid).map_err(|error| error.to_string())?;
    Ok(!members.is_empty())
}

///
/// A supervisor can die by `SIGKILL` before it removes its pid record, leaving the
/// shell and the command it started in its process group. A later start calls this
/// for every recorded run before it opens; the run boundary calls it before the next
/// run starts. The group is believed only when a member still carries the run's own
/// inherited capability or recorded shell identity, so a reused pid or a path in
/// arbitrary argv is never signalled.
///
/// `Ok(true)` means a group was found and stopped; `Ok(false)` means there was
/// nothing left. `Err` is one plain sentence for the card when a process survives.
fn reap_leftover_run(run_dir: &Path, grace: std::time::Duration) -> Result<bool, String> {
    reap_leftover_run_inner(run_dir, grace).map_err(categorize_cleanup)
}

/// Give every cleanup failure one of the three honest categories.
///
/// A reason that already says `live leftover:` or `unverified leftover:` keeps
/// that category. Everything else is `cleanup unverified:`: a missing/read/ending
/// error is uncertainty, never a claim that a live process was seen.
fn categorize_cleanup(reason: String) -> String {
    if reason.starts_with("live leftover:")
        || reason.starts_with("unverified leftover:")
        || reason.starts_with("cleanup unverified:")
    {
        reason
    } else {
        format!("cleanup unverified: {reason}")
    }
}

/// Close every live group that still holds this run's inherited capability.
///
/// The capability is kernel-owned and cannot be copied into the run by name. A
/// group is accepted only after `capability_group` has verified the descriptor
/// (dev/ino/token) of a live member; `reap_process_group` then applies the same
/// pidfd/boot/start ownership checks as the leader path.
fn reap_capability_groups(run_dir: &Path, grace: std::time::Duration) -> Result<bool, String> {
    let mut seen = Vec::new();
    let mut reaped = false;
    for _ in 0..32 {
        let Some(group) = capability_group(run_dir)? else {
            return Ok(reaped);
        };
        if seen.contains(&group) {
            return Err(format!(
                "live leftover: capability-holding group {group} survived reaping"
            ));
        }
        seen.push(group);
        reaped |= reap_process_group(group, run_dir, grace)?;
    }
    Err("live leftover: too many capability-holding groups survived reaping".to_owned())
}

fn reap_leftover_run_inner(run_dir: &Path, grace: std::time::Duration) -> Result<bool, String> {
    // A permissions failure is a recovery failure, not "no records": it must
    // reach the card and block a replacement run (review-3).
    process_records_readable(run_dir)?;
    // Pin the first ending before recovery can signal anything or write an ending.
    // A run whose own terminal record already exists keeps it; otherwise this crash
    // recovery is the first stop and owns the one stopped outcome.
    let pinned_run_id = run_id_of(run_dir)?;
    let proposed = if transcript::terminal_record_checked(run_dir)
        .map_err(|error| format!("the run's ending cannot be read: {error}"))?
        .is_some()
    {
        FirstEnding::Terminal
    } else {
        FirstEnding::Stop
    };
    // Recovery must still stop a live child when the fact cannot be written. The
    // fact error is returned at the boundary, keeping the identity handles, so a
    // later retry can record the ending instead of claiming one now.
    let pin_error = ensure_first_ending(run_dir, pinned_run_id, proposed).err();
    // A live shell identity names the group even if an old `session.*` pair has
    // been overtaken by pid reuse. `recorded_group` stays the supervisor-first
    // record for the private run-temp path. The inherited anonymous capability
    // finds the group in the pre-record window and when records were lost.
    let live_shell = match read_shell_record(run_dir) {
        Some(record) => {
            match shell_identity_is_live(run_dir, record.pgid).map_err(|error| error.to_string())? {
                true => Some(record.pgid),
                false => None,
            }
        }
        None => None,
    };
    let pgid = match live_shell {
        Some(pgid) => Some(pgid),
        None => match recorded_group(run_dir) {
            Some((_, pgid)) => Some(pgid),
            None => capability_group(run_dir)?,
        },
    };
    let has_capability = run_dir.join(SHELL_CAPABILITY_FILE).is_file();
    let Some(pgid) = pgid else {
        match transcript::terminal_record_checked(run_dir) {
            Ok(Some(_)) => {
                // A supervisor's outcome is not cleanup proof. A capability record
                // means some process inherited the run's anonymous handle; if no
                // live holder was verified (and the scan did not report an
                // inaccessible same-user process), the record still cannot be
                // discarded on the strength of the ending alone. A record from
                // before this boot cannot have a live process, so that bounded
                // case is closed.
                if has_capability && !run_predates_boot(run_dir) {
                    return Err(
                        "cleanup unverified: the run has an ending but its capability record has no verified holder"
                            .to_owned(),
                    );
                }
                // The durable first-ending fact decides whether this recovery
                // replaces a post-stop child ending or leaves a terminal-first
                // ending alone. write_run_stopped reads that fact.
                if let Some(run_id) = run_dir.file_name().and_then(std::ffi::OsStr::to_str) {
                    write_run_stopped(
                        run_dir,
                        run_id,
                        0,
                        "the run's first stop was still in flight when the engine reopened",
                    )
                    .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
                }
                if let Some(error) = pin_error.clone() {
                    return Err(error);
                }
                clear_process_records(run_dir);
                return Ok(false);
            }
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "cleanup unverified: the run's ending cannot be read, so cleanup cannot be verified: {error}"
                ));
            }
        }
        // No process record, identity or verified holder. A capability record is
        // durable evidence that spawn may have happened, so it is start evidence
        // even without a transcript. A run that never started is stale and can
        // go; a started/uncertain run is bounded by the boot.
        if run_started(run_dir)? || has_capability {
            if run_predates_boot(run_dir) {
                if let Some(run_id) = run_dir.file_name().and_then(std::ffi::OsStr::to_str) {
                    write_run_stopped(
                        run_dir,
                        run_id,
                        0,
                        "the run predates this boot; no process from it can be alive",
                    )
                    .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
                }
                if let Some(error) = pin_error.clone() {
                    return Err(error);
                }
                clear_process_records(run_dir);
                return Ok(false);
            }
            if has_capability {
                return Err(
                    "cleanup unverified: a capability record remains but no live holder can be verified; cleanup is uncertain"
                        .to_owned(),
                );
            }
            return Err(
                "cleanup unverified: the run started but its process records are lost; cleanup cannot be verified".to_owned(),
            );
        }
        if let Some(error) = pin_error.clone() {
            return Err(error);
        }
        clear_process_records(run_dir);
        return Ok(false);
    };
    let mut reaped = reap_process_group(pgid, run_dir, grace)?;
    reaped |= reap_capability_groups(run_dir, grace)?;
    if !reaped && unverified_group(pgid)? {
        // A live group whose ownership cannot be established is not touched, but
        // it is not hidden either: the open reports it, the card names it and
        // Start refuses a replacement until it is resolved.
        return Err(format!(
            "unverified leftover: a live process group (pgid {pgid}) cannot be verified as this run's; it was not signalled"
        ));
    }
    if !reaped && !unverified_group(pgid)? {
        match transcript::terminal_record_checked(run_dir) {
            Ok(Some(_)) => {
                // Honor the durable fact even when the child wrote an ending
                // during the first stop's grace: Stop replaces it; Terminal keeps
                // it and writes no marker.
                if let Some(run_id) = run_dir.file_name().and_then(std::ffi::OsStr::to_str) {
                    write_run_stopped(
                        run_dir,
                        run_id,
                        pgid,
                        "the run's first stop was still in flight when the engine reopened",
                    )
                    .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
                }
            }
            Ok(None) => {
                // The recorded numbers survived but nothing is alive: the supervisor
                // died before it could close the run. Give it the ending the reopen
                // is owed, durably, before its handles are removed (F-02).
                if let Some(run_id) = run_dir.file_name().and_then(std::ffi::OsStr::to_str) {
                    write_run_stopped(
                        run_dir,
                        run_id,
                        pgid,
                        "the run from an earlier start ended without a closing record",
                    )
                    .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
                }
            }
            Err(error) => {
                return Err(format!(
                    "the run's ending cannot be read, so cleanup cannot be verified: {error}"
                ));
            }
        }
    }
    if let Some(error) = pin_error {
        return Err(error);
    }
    clear_process_records(run_dir);
    Ok(reaped)
}

/// Stop one run's process group by the group id the engine recorded at spawn.
///
/// This is the boundary call: the supervisor may already have been reaped and its
/// pid files removed, but the shell record it wrote still names the group it
/// started. The group is believed only through the run's own records, so a
/// recycled group id is never signalled.
pub fn reap_process_group(
    pgid: u32,
    run_dir: &Path,
    grace: std::time::Duration,
) -> Result<bool, String> {
    let Some(run_id) = run_dir.file_name().and_then(std::ffi::OsStr::to_str) else {
        return Ok(false);
    };
    let mut known: Vec<KnownMember> = Vec::new();
    let (targets, blocked) = observe_run_group(pgid, run_dir, &mut known);
    if let Some(reason) = blocked {
        return Err(reason);
    }
    if targets.is_empty() {
        if unverified_group(pgid)? {
            return Err(format!(
                "unverified leftover: a live process group (pgid {pgid}) cannot be verified as this run's; it was not signalled"
            ));
        }
        return Ok(false);
    }
    signal_targets(&targets, nix::sys::signal::Signal::SIGTERM)?;
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        let (targets, blocked) = observe_run_group(pgid, run_dir, &mut known);
        if let Some(reason) = blocked {
            return Err(reason);
        }
        if targets.is_empty() {
            if unverified_group(pgid)? {
                return Err(format!(
                    "unverified leftover: a live process group (pgid {pgid}) cannot be verified as this run's; it was not signalled"
                ));
            }
            write_run_stopped(
                run_dir,
                run_id,
                pgid,
                "an earlier start's run was still running and was stopped",
            )
            .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Rebind before the force: every KILL goes through a fresh pidfd whose
    // identity was checked after the handle was opened.
    let (targets, blocked) = observe_run_group(pgid, run_dir, &mut known);
    if let Some(reason) = blocked {
        return Err(reason);
    }
    if targets.is_empty() {
        if unverified_group(pgid)? {
            return Err(format!(
                "unverified leftover: a live process group (pgid {pgid}) cannot be verified as this run's; it was not signalled"
            ));
        }
        write_run_stopped(
            run_dir,
            run_id,
            pgid,
            "an earlier start's run was still running and was stopped",
        )
        .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
        return Ok(true);
    }
    signal_targets(&targets, nix::sys::signal::Signal::SIGKILL)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        let (targets, blocked) = observe_run_group(pgid, run_dir, &mut known);
        if let Some(reason) = blocked {
            return Err(reason);
        }
        if targets.is_empty() {
            if unverified_group(pgid)? {
                return Err(format!(
                    "unverified leftover: a live process group (pgid {pgid}) cannot be verified as this run's; it was not signalled"
                ));
            }
            write_run_stopped(
                run_dir,
                run_id,
                pgid,
                "an earlier start's run was still running and was stopped",
            )
            .map_err(|error| format!("the run's stop record cannot be written: {error}"))?;
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(format!(
        "live leftover: a run process (pid {}) could not be stopped",
        targets.first().map(|target| target.pid).unwrap_or(pgid)
    ))
}

/// The readable transcript's file name, beside the append-only JSONL.
pub const TRANSCRIPT_TEXT_FILE: &str = "transcript.txt";

/// Render one run's JSONL record as a transcript a person can read.
///
/// The JSONL stays the record; this is a view of it. Each line names the turn, the
/// role or tool, and the bounded text; the raw tool output and the model's own words
/// are kept, not the request bodies and tool schemas (F-12).
pub fn readable_transcript(run_dir: &Path) -> String {
    let Ok(file) = transcript::read(run_dir) else {
        return String::new();
    };
    let mut out = String::new();
    for record in &file.records {
        match record {
            Record::RunStarted {
                ts, run_id, agent, ..
            } => out.push_str(&format!("{ts}  run {run_id} of {agent} started\n")),
            Record::InstructionRead {
                ts, path, bytes, ..
            } => out.push_str(&format!("{ts}  instruction read: {path} ({bytes} bytes)\n")),
            Record::SkillsDiscovered { ts, skills } => {
                let names = skills
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{ts}  skills: {names}\n"));
            }
            Record::RunLimits {
                ts,
                max_turns,
                timeout_secs,
                model,
                ..
            } => out.push_str(&format!(
                "{ts}  bounds: {max_turns} turns, {timeout_secs} s, model {model}\n"
            )),
            Record::NativeStarted {
                ts,
                harness,
                version,
                protocol,
                base_url,
                model,
                codex_sandbox,
                ..
            } => out.push_str(&format!(
                "{ts}  native program: {harness} {version}, {protocol} at {base_url}, model {model}{}\n",
                codex_sandbox
                    .as_deref()
                    .map(|mode| format!(", Codex sandbox {mode}"))
                    .unwrap_or_default()
            )),
            Record::NativeText { ts, text } => {
                out.push_str(&format!("{ts}  native: {}\n", bounded(text, 4000)));
            }
            Record::ModelReply {
                ts,
                turn,
                content,
                tool_calls,
                ..
            } => {
                if let Some(content) = content.as_deref().filter(|text| !text.trim().is_empty()) {
                    out.push_str(&format!(
                        "{ts}  turn {turn} model: {}\n",
                        bounded(content, 4000)
                    ));
                }
                for call in tool_calls {
                    out.push_str(&format!(
                        "{ts}  turn {turn} model asks {}({})\n",
                        call.name,
                        bounded(&call.arguments, 600)
                    ));
                }
            }
            Record::ModelCallFailed {
                ts,
                turn,
                class,
                message,
                ..
            } => out.push_str(&format!(
                "{ts}  turn {turn} model failed ({class}): {}\n",
                bounded(message, 800)
            )),
            Record::ToolCall {
                ts,
                turn,
                tool,
                command,
                ..
            } => out.push_str(&format!(
                "{ts}  turn {} {tool}: {}\n",
                turn.map(|turn| turn.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                bounded(command, 600)
            )),
            Record::ToolResult {
                ts,
                tool,
                exit_code,
                signal,
                output,
                ..
            } => {
                let end = match (exit_code, signal) {
                    (Some(code), _) => format!("exit {code}"),
                    (None, Some(signal)) => format!("signal {signal}"),
                    _ => "ended".to_owned(),
                };
                out.push_str(&format!("{ts}  {tool} -> {end}\n"));
                if let Some(output) = output.as_deref().filter(|text| !text.trim().is_empty()) {
                    for line in bounded(output, 4000).lines() {
                        out.push_str(&format!("          {line}\n"));
                    }
                }
            }
            Record::ForgejoCall {
                ts,
                tool,
                repository,
                number,
                detail,
                ..
            } => {
                let target = match (repository, number) {
                    (Some(repository), Some(number)) => format!(" {repository}#{number}"),
                    (Some(repository), None) => format!(" {repository}"),
                    _ => String::new(),
                };
                out.push_str(&format!("{ts}  {tool}{target}: {}\n", bounded(detail, 600)));
            }
            Record::ForgejoResult {
                ts,
                tool,
                outcome,
                action,
                url,
                ..
            } => {
                let say = action.as_deref().unwrap_or(outcome);
                let at = url
                    .as_deref()
                    .map(|url| format!(" {url}"))
                    .unwrap_or_default();
                out.push_str(&format!("{ts}  {tool} -> {say}{at}\n"));
            }
            Record::RunFinished {
                ts,
                outcome,
                reason,
                ..
            } => {
                let reason = reason
                    .as_deref()
                    .map(|reason| format!(" — {}", bounded(reason, 800)))
                    .unwrap_or_default();
                out.push_str(&format!("{ts}  run finished: {outcome}{reason}\n"));
            }
            _ => {}
        }
    }
    out
}

/// Write the readable transcript beside the JSONL at 0600 and return its path.
pub fn write_readable_transcript(run_dir: &Path) -> io::Result<PathBuf> {
    let path = run_dir.join(TRANSCRIPT_TEXT_FILE);
    let text = readable_transcript(run_dir);
    crate::mode::write_private(&path, text.as_bytes())?;
    Ok(path)
}

fn bounded(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push_str(" …");
    }
    out
}

#[cfg(test)]
mod review3_identity_tests {
    use super::*;
    use crate::test_wait::wait_until;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn own_group_process() -> std::process::Child {
        Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the unrelated process starts")
    }

    fn wait_for(pid: u32) {
        wait_until("the unrelated process to become visible", || {
            crate::bundled_forgejo::command_line(pid)
                .is_some_and(|parts| parts.iter().any(|part| part == "30"))
        });
    }

    /// The signal site rechecks the boot/start identity of every known member: an
    /// identity that no longer matches is not selected, even though the pid is in
    /// the observed group.
    #[test]
    fn a_changed_identity_is_not_signalled() {
        let run_dir = crate::test_support::scratch_root("identity-change-signal");
        let mut child = own_group_process();
        let pid = child.id();
        wait_for(pid);
        let (group, identity) = process_identity(pid)
            .expect("identity")
            .expect("a live identity");
        let changed = ProcessIdentity {
            boot: identity.boot.clone(),
            start: identity.start.saturating_add(1),
        };
        let mut known = vec![(pid, changed)];
        let (targets, blocked) = observe_run_group(group, &run_dir, &mut known);
        assert!(blocked.is_none());
        assert!(targets.is_empty(), "a changed identity was selected");
        assert!(
            child.try_wait().expect("status").is_none(),
            "a changed identity was signalled"
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.wait();
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// The owned path (a live shell identity) selects the process and signals it
    /// through the pidfd that was opened before the identity check.
    #[test]
    fn an_owned_identity_is_signalled_through_its_pidfd() {
        let run_dir = crate::test_support::scratch_root("identity-same-signal");
        let mut child = own_group_process();
        let pid = child.id();
        wait_for(pid);
        let (group, _identity) = process_identity(pid)
            .expect("identity")
            .expect("a live identity");
        write_shell_record(&run_dir, pid, group).expect("shell record");
        let mut known = Vec::new();
        let (targets, blocked) = observe_run_group(group, &run_dir, &mut known);
        assert!(blocked.is_none());
        assert_eq!(targets.len(), 1, "the owned process was not selected");
        signal_targets(&targets, nix::sys::signal::Signal::SIGTERM).expect("signal");
        wait_until("the verified process to be signalled", || {
            child.try_wait().expect("status").is_some()
        });
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// A member listed by `group_members()` can be reaped before its pidfd opens.
    /// The kernel then reports EINVAL for a struct pid without a task. That is
    /// absence, not a live process with no handle: recovery must not keep the
    /// leftover and block Start for a process that no longer exists.
    #[test]
    fn a_member_reaped_inside_the_scan_window_is_absent() {
        let run_dir = crate::test_support::scratch_root("pidfd-reaped-window");
        let mut child = own_group_process();
        let pid = child.id();
        wait_for(pid);
        let (group, _identity) = process_identity(pid)
            .expect("identity")
            .expect("a live identity");
        let seam = PidfdReapSeam::arm(pid);
        let mut known = Vec::new();
        let (targets, blocked) = observe_run_group(group, &run_dir, &mut known);
        drop(seam);
        assert!(
            blocked.is_none(),
            "a reaped member was called a live leftover: {blocked:?}"
        );
        assert!(targets.is_empty(), "a reaped member was selected");
        assert!(
            process_facts(pid).expect("the stat is readable").is_none(),
            "the seam did not leave the member reaped"
        );
        let _ = child.wait();
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// The other positive absence the EINVAL recheck accepts is a zombie: it has
    /// ended, and only its parent's reaping is left.
    #[test]
    fn a_zombie_is_an_ending_fact_for_the_einval_recheck() {
        let mut child = own_group_process();
        let pid = child.id();
        wait_for(pid);
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        wait_until(
            "the child to reach an ended state",
            || matches!(process_facts(pid), Ok(Some((state, _, _))) if process_state_has_ended(state)),
        );
        assert!(
            pidfd_einval_is_absence(pid),
            "a zombie was not a positive ending fact"
        );
        let _ = child.wait();
    }
}

#[cfg(test)]
mod stop_record_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-stop-record-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("scratch root");
        root
    }

    fn run_with_terminal(root: &Path, outcome: RunOutcome, reason: &str) -> PathBuf {
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let mut transcript = transcript::Transcript::open(&run).expect("transcript");
        transcript
            .append_terminal(
                &Record::run_finished(
                    "run-1700000200000-0001",
                    outcome,
                    None,
                    10,
                    Some(reason.to_owned()),
                ),
                &ending::seal(),
            )
            .expect("terminal record");
        run
    }

    fn stop_reason(run: &Path) -> String {
        let text = fs::read_to_string(run.join(STOPPED_FILE)).expect("run-stopped.json");
        let record: RunStopped = serde_json::from_str(&text).expect("stop record JSON");
        record.reason
    }

    fn terminal(run: &Path) -> Record {
        transcript::terminal_record(run).expect("terminal record")
    }

    #[test]
    fn a_timed_out_run_records_the_stop_and_keeps_the_last_cause() {
        let root = scratch("timeout");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let reason = "the run reached its time limit and was stopped; last reported cause: the native provider reported 500: synthetic status 500";
        let settled = settle_run_with_stop(
            &run,
            4_000_000,
            RunOutcome::TimedOut,
            Some(reason.to_owned()),
            Duration::from_millis(50),
            Some(reason.to_owned()),
            FirstEnding::Stop,
        )
        .expect("the timeout settles");
        assert!(!settled);
        let marker = stop_reason(&run);
        assert!(marker.contains("time limit"), "{marker}");
        assert!(marker.contains("synthetic status 500"), "{marker}");
        match terminal(&run) {
            Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            } => {
                assert_eq!(outcome, "timed_out");
                assert!(reason.contains("synthetic status 500"), "{reason}");
            }
            other => panic!("the timeout did not write one ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_terminal_record_that_predates_the_stop_keeps_the_one_outcome() {
        let root = scratch("finish-then-stop");
        let run = run_with_terminal(
            &root,
            RunOutcome::Failed,
            "the native provider reported AUTH: 401 unauthorized",
        );
        settle_run_with_stop(
            &run,
            4_000_001,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(50),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Terminal,
        )
        .expect("the stop settles");
        assert!(
            !run.join(STOPPED_FILE).exists(),
            "a stop after the run's ending wrote a false stop record"
        );
        match terminal(&run) {
            Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            } => {
                assert_eq!(outcome, "failed");
                assert!(reason.contains("401 unauthorized"), "{reason}");
            }
            other => panic!("the earlier ending was replaced: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stop_that_wins_replaces_a_child_ending_written_after_the_signal() {
        let root = scratch("stop-then-finish");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the run finished while the stop was in flight",
        );
        settle_run_with_stop(
            &run,
            4_000_002,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(50),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Stop,
        )
        .expect("the stop settles");
        let marker = stop_reason(&run);
        assert!(marker.contains("stopped on request"), "{marker}");
        let records = transcript::read(&run).expect("transcript").records;
        let terminals: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .collect();
        assert_eq!(terminals.len(), 1, "the run has more than one ending");
        match terminals[0] {
            Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            } => {
                assert_eq!(outcome, "stopped");
                assert!(reason.contains("stopped on request"), "{reason}");
            }
            other => panic!("the stop did not own the one ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recovery_stop_keeps_the_existing_single_ending() {
        let root = scratch("recovery-existing");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the run finished before the reopen",
        );
        write_run_stopped(
            &run,
            "run-1700000200000-0001",
            4_000_003,
            "an earlier start's leftover group was stopped",
        )
        .expect("the recovery stop record");
        let records = transcript::read(&run).expect("transcript").records;
        let terminals: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .collect();
        assert_eq!(terminals.len(), 1, "recovery wrote a second ending");
        match terminals[0] {
            Record::RunFinished { outcome, .. } => assert_eq!(outcome, "succeeded"),
            other => panic!("unexpected ending: {other:?}"),
        }
        assert!(
            run.join(STOPPED_FILE).is_file(),
            "the cleanup was not recorded"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_persisted_stop_replaces_a_child_ending_on_recovery() {
        let root = scratch("recovery-persisted-stop");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the child finished during the first stop's grace",
        );
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("the first ending");
        write_run_stopped(
            &run,
            run_id,
            4_000_004,
            "the first stop was still in flight when the engine reopened",
        )
        .expect("the recovery stop record");
        assert!(run.join(STOPPED_FILE).is_file());
        let records = transcript::read(&run).expect("transcript").records;
        let terminals: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .collect();
        assert_eq!(terminals.len(), 1, "recovery kept two endings");
        match terminals[0] {
            Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            } => {
                assert_eq!(outcome, "stopped");
                assert!(reason.contains("still in flight"), "{reason}");
            }
            other => panic!("the persisted stop did not own the ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reopen_after_a_persisted_stop_replaces_the_child_ending() {
        let root = scratch("reopen-persisted-stop");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the child finished during the first stop's grace",
        );
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("the first ending");
        let reaped =
            reap_leftover_run(&run, Duration::from_millis(20)).expect("the reopen settles the run");
        assert!(!reaped);
        assert!(run.join(STOPPED_FILE).is_file());
        let records = transcript::read(&run).expect("transcript").records;
        let terminals: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .collect();
        assert_eq!(terminals.len(), 1, "the reopen kept two endings");
        match terminals[0] {
            Record::RunFinished { outcome, .. } => assert_eq!(outcome, "stopped"),
            other => panic!("the reopen did not write one stopped ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stopped_run_keeps_its_real_elapsed_time() {
        let root = scratch("stop-duration");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        let mut transcript = transcript::Transcript::open(&run).expect("transcript");
        transcript
            .append(&Record::run_started(
                run_id,
                "agent-1",
                std::process::id(),
                &run.display().to_string(),
            ))
            .expect("started record");
        std::thread::sleep(std::time::Duration::from_millis(20));
        settle_run_with_stop(
            &run,
            4_000_123,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(20),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Stop,
        )
        .expect("the stop settles");
        match terminal(&run) {
            Record::RunFinished { duration_ms, .. } => {
                assert!(
                    duration_ms >= 20,
                    "a stopped run wrote duration_ms {duration_ms}"
                );
            }
            other => panic!("no stopped ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reopen_keeps_an_already_stopped_ending() {
        let root = scratch("reopen-already-stopped");
        let run = run_with_terminal(
            &root,
            RunOutcome::Stopped,
            "the run was stopped before it became current",
        );
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("the first ending");
        let before = terminal(&run);
        let reaped =
            reap_leftover_run(&run, Duration::from_millis(20)).expect("the reopen settles the run");
        assert!(!reaped);
        let after = terminal(&run);
        match (&before, &after) {
            (
                Record::RunFinished {
                    ts: before_ts,
                    reason: before_reason,
                    ..
                },
                Record::RunFinished {
                    ts: after_ts,
                    reason: after_reason,
                    ..
                },
            ) => {
                assert_eq!(
                    before_ts, after_ts,
                    "the reopen changed an already stopped ending's timestamp"
                );
                assert_eq!(
                    before_reason, after_reason,
                    "the reopen rewrote an already stopped ending"
                );
            }
            _ => panic!("the run's one terminal record changed kind"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_persisted_terminal_recovery_keeps_the_ending_and_writes_no_marker() {
        let root = scratch("recovery-persisted-terminal");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the run finished before the stop",
        );
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Terminal).expect("the first ending");
        write_run_stopped(
            &run,
            run_id,
            4_000_005,
            "a leftover group was cleaned after the ending",
        )
        .expect("the recovery cleanup");
        assert!(
            !run.join(STOPPED_FILE).is_file(),
            "a terminal-first recovery wrote a false stop record"
        );
        match terminal(&run) {
            Record::RunFinished { outcome, .. } => assert_eq!(outcome, "succeeded"),
            other => panic!("the ending changed: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_persisted_first_ending_is_write_once() {
        let root = scratch("first-ending-write-once");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        assert_eq!(
            ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("first"),
            FirstEnding::Stop
        );
        assert_eq!(
            ensure_first_ending(&run, run_id, FirstEnding::Terminal).expect("second"),
            FirstEnding::Stop,
            "a later writer changed the first ending"
        );
        assert_eq!(
            recorded_first_ending(&run).expect("read"),
            Some(FirstEnding::Stop)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_terminal_fact_without_its_terminal_is_uncertain() {
        let root = scratch("terminal-fact-missing");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Terminal).expect("terminal fact");
        // A stop that loses the first-ending arbitration to Terminal must not
        // signal the supervisor that is writing its own run_finished.
        struct RecordingSession {
            signalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl RunningSession for RecordingSession {
            fn pid(&self) -> u32 {
                4_000_099
            }
            fn try_status(&mut self) -> Option<std::process::ExitStatus> {
                None
            }
            fn signal(&mut self, _force: bool) {
                self.signalled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let signalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handle = RunHandle {
            inner: Box::new(RecordingSession {
                signalled: std::sync::Arc::clone(&signalled),
            }),
        };
        let ending = request_stop(&mut handle, &run, run_id, FirstEnding::Stop)
            .expect("the existing Terminal fact wins");
        assert_eq!(ending, FirstEnding::Terminal);
        assert!(
            !signalled.load(std::sync::atomic::Ordering::SeqCst),
            "a stop signalled a supervisor whose terminal fact had already won"
        );

        // The pre-handle start path has only the reported pid, but it must consult
        // the same Terminal fact before signalling a finishing supervisor.
        use std::os::unix::process::CommandExt;
        let mut raw = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the pre-handle child starts");
        let raw_pid = raw.id();
        request_starting_stop(raw_pid, Some(&run), false, "the starting run was stopped");
        // Observe the process, not a zombie: a signalled child may still answer
        // /proc until it is reaped. The product's own liveness check skips zombies.
        let running = || {
            std::fs::read_to_string(format!("/proc/{raw_pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .and_then(|rest| rest.split_whitespace().next().map(|state| state != "Z"))
                })
                .unwrap_or(false)
        };
        let observe_until = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut raw_running = true;
        while std::time::Instant::now() < observe_until {
            raw_running = running();
            if !raw_running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(raw_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = raw.wait();
        assert!(
            raw_running,
            "the pre-handle stop signalled a supervisor whose terminal fact had already won"
        );

        let error = settle_run_with_stop(
            &run,
            4_000_020,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(20),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Terminal,
        )
        .expect_err("a terminal fact without its ending must be uncertain");
        assert!(error.contains("uncertain"), "{error}");
        assert!(
            !run.join(STOPPED_FILE).is_file(),
            "a clean stop was claimed"
        );
        assert!(run.join(ENDING_UNCERTAIN_FILE).is_file());
        match transcript::terminal_record(&run) {
            None => {}
            Some(other) => panic!("an ending was invented: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_foreign_first_ending_is_uncertain() {
        let root = scratch("foreign-first-ending");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        let text = serde_json::json!({
            "ts": "2026-10-24T17:00:00.000Z",
            "run_id": "run-9999999999999-9999",
            "ending": "stop"
        })
        .to_string();
        fs::write(run.join(FIRST_ENDING_FILE), text).expect("foreign fact");
        let error = recorded_first_ending(&run).expect_err("a foreign fact is not this run's");
        assert!(error.contains("belongs to run"), "{error}");
        assert!(ending_uncertainty(&run).expect("read").is_none());
        let error = ensure_first_ending(&run, run_id, FirstEnding::Stop)
            .expect_err("a foreign fact cannot be re-used");
        assert!(error.contains("belongs to run"), "{error}");
        assert!(
            ending_uncertainty(&run).expect("read").is_some(),
            "the foreign fact did not persist uncertainty evidence"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_uncertainty_record_blocks_clean_recovery() {
        let root = scratch("uncertainty-blocks");
        let run = run_with_terminal(
            &root,
            RunOutcome::Succeeded,
            "the child finished before the stop",
        );
        let run_id = "run-1700000200000-0001";
        record_ending_uncertainty(&run, run_id, "the first-ending write failed")
            .expect("uncertainty record");
        let error = reap_leftover_run(&run, Duration::from_millis(20))
            .expect_err("an uncertain ending must not recover cleanly");
        assert!(
            error.contains("uncertain") || error.contains("cannot be read"),
            "{error}"
        );
        assert!(
            !run.join(STOPPED_FILE).is_file(),
            "a clean stop was claimed"
        );
        match terminal(&run) {
            Record::RunFinished { outcome, .. } => assert_eq!(outcome, "succeeded"),
            other => panic!("the ending changed: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_natural_writer_after_a_stop_fact_adds_no_terminal() {
        let root = scratch("natural-after-stop");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("stop fact");
        request_natural_finish(
            &run,
            run_id,
            RunOutcome::Succeeded,
            Some(0),
            10,
            Some("the child finished after the stop".to_owned()),
        )
        .expect("the natural writer returns without a terminal");
        assert!(
            transcript::terminal_record(&run).is_none(),
            "a Stop fact was followed by a natural terminal"
        );
        request_finish(
            &run,
            4_000_030,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(20),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Stop,
        )
        .expect("the stop settles");
        let records = transcript::read(&run).expect("transcript").records;
        let terminals: Vec<&Record> = records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .collect();
        assert_eq!(terminals.len(), 1);
        match terminals[0] {
            Record::RunFinished { outcome, .. } => assert_eq!(outcome, "stopped"),
            other => panic!("unexpected ending: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_panicking_spawn_still_records_the_prepared_ending() {
        let root = scratch("panicking-spawn");
        let run = root.join("run-1700000200000-0001");
        let guard =
            ending::Owner::prepare(&run, "run-1700000200000-0001", &root).expect("prepare the run");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = guard.start(&mut |_| {}, |_, _, _| panic!("fixture spawn panic"));
        }));
        assert!(result.is_err(), "the fixture did not panic");
        assert!(
            run.join(STOPPED_FILE).is_file(),
            "no stop record after the panic"
        );
        let terminal = transcript::terminal_record(&run).expect("no terminal after the panic");
        assert!(matches!(
            terminal,
            Record::RunFinished { ref outcome, .. } if outcome == "stopped"
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_panicking_spawn_after_a_child_is_reported_stops_the_child() {
        let root = scratch("panicking-spawn-with-child");
        let run = root.join("run-1700000200000-0001");
        let child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("fixture child");
        let pid = child.id();
        drop(child);
        let guard =
            ending::Owner::prepare(&run, "run-1700000200000-0001", &root).expect("prepare the run");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = guard.start(&mut |_| {}, |_, _, on_pid| {
                on_pid(pid);
                panic!("fixture spawn panic after the child");
            });
        }));
        assert!(result.is_err(), "the fixture did not panic");
        assert!(!process_alive(pid), "the reported child survived the panic");
        let terminal = transcript::terminal_record(&run).expect("no terminal after the panic");
        let reason = match terminal {
            Record::RunFinished {
                outcome, reason, ..
            } => {
                assert_eq!(outcome, "stopped");
                reason.unwrap_or_default()
            }
            _ => panic!("not a terminal"),
        };
        assert!(!reason.contains("no child was started"), "{reason}");
        assert!(run.join(STOPPED_FILE).is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prepared_run_cancelled_before_spawn_gets_one_stopped_ending() {
        let root = scratch("cancel-prepared");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        request_cancel_prepared(&run, "run-1700000200000-0001", "the run was called off")
            .expect("cancel the prepared run");
        assert!(run.join(STOPPED_FILE).is_file());
        let terminal = transcript::terminal_record(&run).expect("terminal");
        assert!(matches!(
            terminal,
            Record::RunFinished { ref outcome, .. } if outcome == "stopped"
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_writer_after_a_terminal_fact_without_terminal_is_uncertain() {
        let root = scratch("failed-after-terminal-missing");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Terminal).expect("terminal fact");
        let error = request_failed_finish(&run, run_id, "the child never started")
            .expect_err("a failed terminal cannot be invented");
        assert!(
            error.contains("uncertain") || error.contains("missing"),
            "{error}"
        );
        assert!(transcript::terminal_record(&run).is_none());
        assert!(ending_uncertainty(&run).expect("read").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_writer_after_a_stop_fact_adds_no_terminal() {
        let root = scratch("failed-after-stop");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        ensure_first_ending(&run, run_id, FirstEnding::Stop).expect("stop fact");
        request_failed_finish(&run, run_id, "the child never started")
            .expect("the failed writer returns without a terminal");
        assert!(
            transcript::terminal_record(&run).is_none(),
            "a Stop fact was followed by a failed terminal"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_uncertainty_writers_keep_the_first_evidence() {
        let root = scratch("uncertainty-concurrent");
        let run = root.join("run-1700000200000-0001");
        fs::create_dir_all(&run).expect("run dir");
        let run_id = "run-1700000200000-0001";
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for error in ["first writer", "second writer"] {
            let run = run.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                record_ending_uncertainty(&run, run_id, error)
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread").expect("writer");
        }
        let evidence = ending_uncertainty(&run).expect("read").expect("evidence");
        assert!(
            evidence == "first writer" || evidence == "second writer",
            "{evidence}"
        );
        let records = fs::read_to_string(run.join(ENDING_UNCERTAIN_FILE)).expect("file");
        assert!(records.contains(&evidence));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_reap_still_writes_a_truthful_stop_marker() {
        let root = scratch("reap-failure");
        let run = root.join("run-1700000200000-0003");
        fs::create_dir_all(&run).expect("run dir");
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("unrelated group starts");
        let pid = child.id();
        fs::write(run.join("session.pid"), pid.to_string()).expect("session pid");
        fs::write(run.join("session.pgid"), pid.to_string()).expect("session pgid");
        std::thread::sleep(Duration::from_millis(50));
        let error = settle_run_with_stop(
            &run,
            pid,
            RunOutcome::Stopped,
            Some("the run was stopped on request".to_owned()),
            Duration::from_millis(80),
            Some("the run was stopped on request".to_owned()),
            FirstEnding::Stop,
        )
        .expect_err("an unverified group cannot be claimed stopped");
        assert!(
            error.contains("unverified") || error.contains("cannot be verified"),
            "the reap failure was not named: {error}"
        );
        let marker = stop_reason(&run);
        assert!(marker.contains("stopped on request"), "{marker}");
        assert!(
            marker.contains("could not be verified as stopped"),
            "the stop marker hid the failed reap: {marker}"
        );
        match transcript::terminal_record(&run) {
            Some(Record::RunFinished {
                outcome,
                reason: Some(reason),
                ..
            }) => {
                assert_eq!(outcome, "stopped");
                assert!(
                    reason.contains("could not be verified as stopped"),
                    "the terminal ending hid the failed reap: {reason}"
                );
            }
            other => panic!("the unverified stop wrote no truthful ending: {other:?}"),
        }
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.wait();
        let _ = fs::remove_dir_all(&root);
    }
}
