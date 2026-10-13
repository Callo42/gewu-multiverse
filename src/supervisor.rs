//! The supervisor: the part of the tool that starts, watches and stops agents.
//!
//! The terminal UI owns exactly one supervisor. It starts one child process per
//! agent, reads that process's state, stops it when asked, and stops it when it
//! overruns its timeout. It also holds the concurrency limit: at most a configured
//! number of agents run at once, and a start beyond that limit waits its turn
//! instead of failing.
//!
//! Two rules come straight from how this tool is meant to behave.
//!
//! - **One child process per agent, in its own process group.** The child is started
//!   with the agent's workspace as its working directory and as the leader of a new
//!   process group, so signalling the group reaches the tools the agent started. A
//!   stop is polite first and forceful after a grace, and the group is swept again
//!   when the run ends, so a run leaves nothing of itself behind.
//! - **The process decides, not its output.** An exit of zero is a success; any other
//!   exit is a failure; a run the tool stopped is `stopped`; a run the tool stopped
//!   for its timeout is `timed_out`. What the process wrote is a log, never a verdict.
//!
//! Nothing here runs as root, installs a service or uses a container runtime. The
//! child is an ordinary process of the same user.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::paths::AgentPaths;
use crate::run::{LOG_FILE, RunOutcome, RunPlan, new_run_id};
use crate::transcript::{self, Record, Transcript};

/// How long a stopped run may take to end politely before it is ended forcefully.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// The most significant part of what the screens show about one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Nothing is running and nothing has run in this session.
    Idle,
    /// The start is waiting for a free slot, behind `position` starts.
    Waiting {
        /// The identity the run will have.
        run_id: String,
        /// Its place in the line, starting at one.
        position: usize,
    },
    /// The process was started and has not been seen running yet.
    Starting {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
    },
    /// The process is running.
    Running {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
        /// How long it has been running, in milliseconds.
        elapsed_ms: u64,
    },
    /// A stop was asked for and the process is being ended.
    Stopping {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
        /// How long it has been running, in milliseconds.
        elapsed_ms: u64,
    },
    /// The last run of this agent is over.
    Finished {
        /// The run's identity.
        run_id: String,
        /// How it ended.
        outcome: RunOutcome,
        /// Why, when there is a reason to give.
        reason: Option<String>,
        /// How long it lasted, in milliseconds.
        duration_ms: u64,
    },
}

impl State {
    /// One word for a table cell.
    pub fn word(&self) -> String {
        match self {
            State::Idle => "idle".to_owned(),
            State::Waiting { position, .. } => format!("waiting ({position} in line)"),
            State::Starting { .. } => "starting".to_owned(),
            State::Running { elapsed_ms, .. } => format!("running ({}s)", elapsed_ms / 1000),
            State::Stopping { elapsed_ms, .. } => {
                format!("stopping ({}s)", elapsed_ms / 1000)
            }
            State::Finished { outcome, .. } => match outcome {
                RunOutcome::Succeeded => "exited 0".to_owned(),
                RunOutcome::Failed => "failed".to_owned(),
                RunOutcome::Stopped => "stopped".to_owned(),
                RunOutcome::TimedOut => "timed out".to_owned(),
            },
        }
    }

    /// The run this state is about, if any.
    pub fn run_id(&self) -> Option<&str> {
        match self {
            State::Idle => None,
            State::Waiting { run_id, .. }
            | State::Starting { run_id, .. }
            | State::Running { run_id, .. }
            | State::Stopping { run_id, .. }
            | State::Finished { run_id, .. } => Some(run_id),
        }
    }

    /// Whether this state means a process is alive right now.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            State::Starting { .. } | State::Running { .. } | State::Stopping { .. }
        )
    }
}

/// What happened when a start was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDisposition {
    /// The process was started.
    Started {
        /// The run's identity.
        run_id: String,
        /// The process's id.
        pid: u32,
    },
    /// The start is waiting for a free slot.
    Queued {
        /// The run's identity it will have.
        run_id: String,
        /// Its place in the line, starting at one.
        position: usize,
    },
    /// This agent already has a run, so nothing was started.
    Busy {
        /// The run that is already there.
        run_id: String,
    },
}

/// What happened when a stop was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDisposition {
    /// A running process was told to stop.
    Signalled,
    /// A start that had not begun was called off.
    Cancelled,
    /// There was nothing to stop.
    Nothing,
}

/// Why a start could not be prepared or spawned.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The agent's own directories could not be prepared.
    #[error("{0}")]
    Paths(#[from] crate::paths::PathError),
    /// The run's own directory could not be made, or its plan could not be written.
    #[error("the run {run_id} for `{agent}` cannot be prepared: {source}")]
    Prepare {
        /// The agent.
        agent: String,
        /// The run.
        run_id: String,
        /// The reason from the filesystem.
        #[source]
        source: std::io::Error,
    },
    /// The agent process could not be started.
    #[error("the agent process for `{agent}` cannot be started: {source}")]
    Spawn {
        /// The agent.
        agent: String,
        /// The reason from the operating system.
        #[source]
        source: std::io::Error,
    },
    /// The program does not know which executable to start.
    #[error("the tool's own executable cannot be found, so no agent process can be started")]
    NoBinary,
}

/// A run that is alive right now.
#[derive(Debug)]
struct Live {
    run_id: String,
    dir: PathBuf,
    child: Child,
    pid: u32,
    started: Instant,
    stopping: Option<StopKind>,
    /// When the polite signal was sent, so a forceful one can follow.
    signalled_at: Option<Instant>,
    log: fs::File,
}

/// Why a run is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    /// Someone asked for it.
    Requested,
    /// The run reached its timeout.
    TimedOut,
}

/// A start that is waiting for a free slot.
#[derive(Debug)]
struct Queued {
    name: String,
    paths: AgentPaths,
    steps: Vec<String>,
    run_id: String,
}

/// The supervisor of every agent of one installation.
#[derive(Debug)]
pub struct Supervisor {
    state_root: PathBuf,
    binary: PathBuf,
    max_concurrent: usize,
    timeout: Duration,
    grace: Duration,
    sequence: u64,
    states: HashMap<String, State>,
    live: HashMap<String, Live>,
    queue: VecDeque<Queued>,
    /// Process groups still being swept after their run ended.
    sweeps: Vec<(u32, Instant)>,
    /// The last thing that went wrong, for the status line.
    last_error: Option<String>,
}

impl Supervisor {
    /// A supervisor over one state root, starting `binary` for each run.
    pub fn new(
        state_root: impl Into<PathBuf>,
        binary: impl Into<PathBuf>,
        max_concurrent: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            state_root: state_root.into(),
            binary: binary.into(),
            max_concurrent: max_concurrent.max(1),
            timeout,
            grace: DEFAULT_GRACE,
            sequence: 0,
            states: HashMap::new(),
            live: HashMap::new(),
            queue: VecDeque::new(),
            sweeps: Vec::new(),
            last_error: None,
        }
    }

    /// How long a stopped run may take to end politely.
    pub fn set_grace(&mut self, grace: Duration) {
        self.grace = grace;
    }

    /// Apply the limits a freshly read configuration carries.
    ///
    /// The limit is read here rather than remembered per start, so raising it lets
    /// queued starts go; lowering it never stops a run that is already alive.
    pub fn set_limits(&mut self, max_concurrent: usize, timeout: Duration) {
        self.max_concurrent = max_concurrent.max(1);
        self.timeout = timeout;
    }

    /// The configured limit on agents running at once.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// The configured per-run timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How many agents are running, or starting, right now.
    pub fn running_count(&self) -> usize {
        self.live.len()
    }

    /// How many starts are waiting for a free slot.
    pub fn waiting_count(&self) -> usize {
        self.queue.len()
    }

    /// What the screens should show for one agent.
    pub fn state(&self, name: &str) -> State {
        self.states.get(name).cloned().unwrap_or(State::Idle)
    }

    /// Every agent the supervisor knows about, and its state.
    pub fn states(&self) -> Vec<(String, State)> {
        let mut all: Vec<(String, State)> = self
            .states
            .iter()
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect();
        all.sort_by(|left, right| left.0.cmp(&right.0));
        all
    }

    /// The last problem, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Ask for one run of one agent.
    ///
    /// Beyond the concurrency limit the start does not fail: it waits its turn, and
    /// the poll that follows starts it when a slot is free.
    pub fn start(
        &mut self,
        agent: &AgentPaths,
        steps: Vec<String>,
    ) -> Result<StartDisposition, StartError> {
        let name = agent.name().to_owned();
        if let Some(state) = self.states.get(&name) {
            if state.is_live() {
                let run_id = state.run_id().unwrap_or("unknown").to_owned();
                return Ok(StartDisposition::Busy { run_id });
            }
        }
        if self.queue.iter().any(|queued| queued.name == name) {
            let run_id = self
                .queue
                .iter()
                .find(|queued| queued.name == name)
                .map(|queued| queued.run_id.clone())
                .unwrap_or_default();
            return Ok(StartDisposition::Busy { run_id });
        }
        if self.binary.as_os_str().is_empty() {
            return Err(StartError::NoBinary);
        }

        let run_id = self.next_run_id();
        if self.running_count() >= self.max_concurrent {
            self.queue.push_back(Queued {
                name: name.clone(),
                paths: agent.clone(),
                steps,
                run_id: run_id.clone(),
            });
            let position = self.queue.len();
            self.states.insert(
                name,
                State::Waiting {
                    run_id: run_id.clone(),
                    position,
                },
            );
            self.reindex_queue();
            return Ok(StartDisposition::Queued { run_id, position });
        }
        let pid = self.spawn(agent, steps, run_id.clone())?;
        Ok(StartDisposition::Started { run_id, pid })
    }

    /// Ask one agent to stop. A start still waiting is called off.
    pub fn stop(&mut self, name: &str) -> StopDisposition {
        if let Some(queued_at) = self.queue.iter().position(|queued| queued.name == name) {
            let queued = self
                .queue
                .remove(queued_at)
                .expect("the position was just found");
            self.states.insert(
                name.to_owned(),
                State::Finished {
                    run_id: queued.run_id,
                    outcome: RunOutcome::Stopped,
                    reason: Some("the start was called off before a slot was free".to_owned()),
                    duration_ms: 0,
                },
            );
            self.reindex_queue();
            return StopDisposition::Cancelled;
        }
        let Some(live) = self.live.get_mut(name) else {
            return StopDisposition::Nothing;
        };
        live.stopping = Some(StopKind::Requested);
        live.signalled_at = Some(Instant::now());
        let (pid, run_id) = (live.pid, live.run_id.clone());
        signal_group(pid, Signal::SIGTERM);
        write_log_line(
            &mut live.log,
            &format!("supervisor: a stop was asked for, SIGTERM sent to process group {pid}"),
        );
        let elapsed_ms = live.started.elapsed().as_millis() as u64;
        self.states.insert(
            name.to_owned(),
            State::Stopping {
                run_id,
                pid,
                elapsed_ms,
            },
        );
        StopDisposition::Signalled
    }

    /// Stop every run and wait, within a budget, until no child is left.
    ///
    /// This is what the tool calls before it leaves. It returns how many runs it
    /// stopped. It proves the children are gone by reaping them.
    pub fn stop_all(&mut self, budget: Duration) -> usize {
        let stopped = self.live.len();
        let names: Vec<String> = self.live.keys().cloned().collect();
        for name in &names {
            if let Some(live) = self.live.get_mut(name) {
                live.stopping = Some(StopKind::Requested);
                live.signalled_at = Some(Instant::now());
                signal_group(live.pid, Signal::SIGTERM);
                let (pid, run_id, elapsed_ms) = (
                    live.pid,
                    live.run_id.clone(),
                    live.started.elapsed().as_millis() as u64,
                );
                self.states.insert(
                    name.clone(),
                    State::Stopping {
                        run_id,
                        pid,
                        elapsed_ms,
                    },
                );
            }
        }
        // Cancel everything still waiting.
        let waiting: Vec<String> = self
            .queue
            .iter()
            .map(|queued| queued.name.clone())
            .collect();
        for name in waiting {
            self.stop(&name);
        }

        let deadline = Instant::now() + budget;
        while !self.live.is_empty() && Instant::now() < deadline {
            self.poll(Instant::now());
            if !self.live.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Anything still alive after the budget is ended forcefully.
        let remaining: Vec<String> = self.live.keys().cloned().collect();
        for name in remaining {
            if let Some(live) = self.live.get_mut(&name) {
                signal_group(live.pid, Signal::SIGKILL);
                let _ = live.child.wait();
                let (run_id, duration_ms) = (
                    live.run_id.clone(),
                    live.started.elapsed().as_millis() as u64,
                );
                self.states.insert(
                    name,
                    State::Finished {
                        run_id,
                        outcome: RunOutcome::Stopped,
                        reason: Some("the tool left before the run ended".to_owned()),
                        duration_ms,
                    },
                );
            }
        }
        self.live.clear();
        stopped
    }

    /// Read every live process once: start what can start, finish what has finished,
    /// and stop what has overrun.
    pub fn poll(&mut self, now: Instant) {
        self.sweep_groups(now);

        let names: Vec<String> = self.live.keys().cloned().collect();
        for name in names {
            self.poll_one(&name, now);
        }

        // Fill free slots from the line, in turn.
        while self.running_count() < self.max_concurrent {
            let Some(queued) = self.queue.pop_front() else {
                break;
            };
            match self.spawn(&queued.paths, queued.steps, queued.run_id.clone()) {
                Ok(_) => {}
                Err(error) => {
                    self.last_error = Some(error.to_string());
                    self.states.insert(
                        queued.name.clone(),
                        State::Finished {
                            run_id: queued.run_id,
                            outcome: RunOutcome::Failed,
                            reason: Some(error.to_string()),
                            duration_ms: 0,
                        },
                    );
                }
            }
        }
        self.reindex_queue();
    }

    /// Read one live process.
    fn poll_one(&mut self, name: &str, now: Instant) {
        let Some(live) = self.live.get_mut(name) else {
            return;
        };
        let elapsed_ms = live.started.elapsed().as_millis() as u64;

        // A run that has overrun its timeout is stopped for that reason.
        if live.stopping.is_none() && live.started.elapsed() >= self.timeout {
            live.stopping = Some(StopKind::TimedOut);
            live.signalled_at = Some(now);
            let pid = live.pid;
            signal_group(pid, Signal::SIGTERM);
            write_log_line(
                &mut live.log,
                &format!(
                    "supervisor: the run reached its timeout of {} s, SIGTERM sent to process group {pid}",
                    self.timeout.as_secs()
                ),
            );
            self.states.insert(
                name.to_owned(),
                State::Stopping {
                    run_id: live.run_id.clone(),
                    pid,
                    elapsed_ms,
                },
            );
        }

        // End a run that will not end politely, once its grace has passed.
        if let (Some(_), Some(signalled_at)) = (live.stopping, live.signalled_at) {
            if now.duration_since(signalled_at) >= self.grace {
                let pid = live.pid;
                signal_group(pid, Signal::SIGKILL);
                live.signalled_at = Some(now);
                write_log_line(
                    &mut live.log,
                    &format!(
                        "supervisor: the run did not end within its grace, SIGKILL sent to process group {pid}"
                    ),
                );
            }
        }

        match live.child.try_wait() {
            Ok(Some(status)) => self.finish(name, status),
            Ok(None) => {
                // Seen alive: from here on the screens say running.
                let state = State::Running {
                    run_id: live.run_id.clone(),
                    pid: live.pid,
                    elapsed_ms,
                };
                if live.stopping.is_none() {
                    self.states.insert(name.to_owned(), state);
                }
            }
            Err(error) => {
                self.last_error = Some(format!("the agent process cannot be read: {error}"));
                let run_id = live.run_id.clone();
                let pid = live.pid;
                let duration_ms = live.started.elapsed().as_millis() as u64;
                let _ = live.child.wait();
                let dir = live.dir.clone();
                self.live.remove(name);
                self.record_outcome(
                    name,
                    &dir,
                    &run_id,
                    RunOutcome::Failed,
                    Some(format!("the agent process cannot be read: {error}")),
                    duration_ms,
                );
                self.sweeps.push((pid, Instant::now() + self.grace));
            }
        }
    }

    /// Finish one run whose process has exited.
    fn finish(&mut self, name: &str, status: std::process::ExitStatus) {
        let Some(mut live) = self.live.remove(name) else {
            return;
        };
        let _ = live.child.wait();
        let duration_ms = live.started.elapsed().as_millis() as u64;
        let pid = live.pid;
        signal_group(pid, Signal::SIGTERM);
        self.sweeps.push((pid, Instant::now() + self.grace));

        // The process decides. A stop the tool asked for is `stopped`; a stop for the
        // timeout is `timed_out`; otherwise the exit code decides.
        let (outcome, reason) = match live.stopping {
            Some(StopKind::Requested) => (
                RunOutcome::Stopped,
                Some("the run was stopped on request".to_owned()),
            ),
            Some(StopKind::TimedOut) => (
                RunOutcome::TimedOut,
                Some(format!(
                    "the run reached its timeout of {} s and was stopped",
                    self.timeout.as_secs()
                )),
            ),
            None => {
                if status.success() {
                    (RunOutcome::Succeeded, None)
                } else {
                    (
                        RunOutcome::Failed,
                        Some(describe_status(&status, &live.run_id)),
                    )
                }
            }
        };
        write_log_line(
            &mut live.log,
            &format!(
                "supervisor: run {} is over: {}",
                live.run_id,
                match &reason {
                    Some(reason) => format!("{} ({reason})", outcome.as_str()),
                    None => outcome.as_str().to_owned(),
                }
            ),
        );
        let run_id = live.run_id.clone();
        let dir = live.dir.clone();
        self.record_outcome(name, &dir, &run_id, outcome, reason, duration_ms);
    }

    /// Record one terminal outcome, unless the run already wrote its own.
    fn record_outcome(
        &mut self,
        name: &str,
        run_dir: &Path,
        run_id: &str,
        outcome: RunOutcome,
        reason: Option<String>,
        duration_ms: u64,
    ) {
        let existing = transcript::terminal_record(run_dir);
        if existing.is_none() {
            if let Ok(mut transcript) = Transcript::open(run_dir) {
                let _ = transcript.append(&Record::run_finished(
                    run_id,
                    outcome,
                    None,
                    duration_ms,
                    reason.clone(),
                ));
            }
        }
        let (outcome, reason, duration_ms) = match existing {
            Some(Record::RunFinished {
                outcome,
                reason,
                duration_ms,
                ..
            }) => (
                RunOutcome::parse(&outcome).unwrap_or(RunOutcome::Failed),
                reason,
                duration_ms,
            ),
            _ => (outcome, reason, duration_ms),
        };
        self.states.insert(
            name.to_owned(),
            State::Finished {
                run_id: run_id.to_owned(),
                outcome,
                reason,
                duration_ms,
            },
        );
    }

    /// Start the child process of one run.
    fn spawn(
        &mut self,
        agent: &AgentPaths,
        steps: Vec<String>,
        run_id: String,
    ) -> Result<u32, StartError> {
        let name = agent.name().to_owned();
        agent.ensure()?;
        let run_dir = agent.run_dir(&run_id);
        fs::create_dir_all(&run_dir).map_err(|source| StartError::Prepare {
            agent: name.clone(),
            run_id: run_id.clone(),
            source,
        })?;
        let plan = RunPlan {
            run_id: run_id.clone(),
            agent: name.clone(),
            workspace: agent.workspace().to_path_buf(),
            instruction: agent.instruction().to_path_buf(),
            skills_dir: agent.skills_dir().to_path_buf(),
            steps,
            timeout_secs: self.timeout.as_secs(),
        };
        plan.write(&run_dir).map_err(|source| StartError::Prepare {
            agent: name.clone(),
            run_id: run_id.clone(),
            source,
        })?;

        let log_path = run_dir.join(LOG_FILE);
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(&log_path)
            .map_err(|source| StartError::Prepare {
                agent: name.clone(),
                run_id: run_id.clone(),
                source,
            })?;
        write_log_line(
            &mut log,
            &format!(
                "supervisor: starting run {run_id} of `{name}` in {}",
                agent.workspace().display()
            ),
        );

        let mut command = Command::new(&self.binary);
        command
            .arg("--run-agent")
            .arg(&run_dir)
            .current_dir(agent.workspace())
            // The agent process writes its own log file, so its own streams are set
            // aside: nothing an agent prints can reach the terminal UI's screen.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A new process group, so one signal reaches the agent and everything it
            // started. The child becomes the leader, so its group id is its own id.
            .process_group(0);
        let child = command.spawn().map_err(|source| StartError::Spawn {
            agent: name.clone(),
            source,
        })?;
        let pid = child.id();
        self.live.insert(
            name.clone(),
            Live {
                run_id: run_id.clone(),
                dir: run_dir,
                child,
                pid,
                started: Instant::now(),
                stopping: None,
                signalled_at: None,
                log,
            },
        );
        self.states.insert(name, State::Starting { run_id, pid });
        Ok(pid)
    }

    /// Escalate the sweep of process groups whose run has ended.
    fn sweep_groups(&mut self, now: Instant) {
        let mut remaining = Vec::new();
        for (pid, deadline) in self.sweeps.drain(..) {
            if now >= deadline {
                signal_group(pid, Signal::SIGKILL);
            } else {
                remaining.push((pid, deadline));
            }
        }
        self.sweeps = remaining;
    }

    /// Number the line again after it changes.
    fn reindex_queue(&mut self) {
        for (at, queued) in self.queue.iter().enumerate() {
            self.states.insert(
                queued.name.clone(),
                State::Waiting {
                    run_id: queued.run_id.clone(),
                    position: at + 1,
                },
            );
        }
    }

    /// The next run id this supervisor will hand out.
    fn next_run_id(&mut self) -> String {
        self.sequence += 1;
        new_run_id(self.sequence)
    }

    /// The directory of a run, for a caller that has only the id.
    pub fn run_dir(&self, name: &str, run_id: &str) -> PathBuf {
        self.state_root.join(name).join(run_id)
    }
}

/// One line for the log.
fn write_log_line(log: &mut fs::File, line: &str) {
    let _ = writeln!(log, "{} {line}", crate::run::started_stamp());
    let _ = log.flush();
}

/// Describe an exit status the tool did not ask for.
fn describe_status(status: &std::process::ExitStatus, run_id: &str) -> String {
    match status.code() {
        Some(code) => format!("run {run_id} exited with code {code}"),
        None => match signal_of(*status) {
            Some(signal) => format!("run {run_id} was ended by signal {signal}"),
            None => format!("run {run_id} ended with no status"),
        },
    }
}

/// The signal that ended a process, when one did.
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Send a signal to a whole process group. A group that is already gone is not an
/// error: the thing this function exists for has already happened.
fn signal_group(pid: u32, signal: Signal) {
    let _ = killpg(Pid::from_raw(pid as i32), signal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::PLAN_FILE;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    /// Write a fake agent process and return its path.
    ///
    /// It is a shell script, not the real binary, because these tests are about the
    /// supervisor's own rules — the limit, the line, the timeout, the signals — and a
    /// script can be told to do exactly one of those things.
    fn fake_agent(tag: &str, body: &str) -> PathBuf {
        let path = scratch_root(tag).join("fake-agent.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the fake is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the fake is runnable");
        path
    }

    fn layout(tag: &str) -> (crate::paths::Layout, AgentPaths) {
        let root = scratch_root(tag);
        let layout = crate::paths::Layout::rooted_at(&root);
        let agent = layout
            .ensure_agent("scribe")
            .expect("the agent is prepared");
        (layout, agent)
    }

    fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_started_run_is_visible_and_then_finishes_cleanly() {
        let binary = fake_agent("sup-ok", "exit 0");
        let (layout, agent) = layout("sup-ok");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        let disposition = supervisor.start(&agent, Vec::new()).expect("started");
        assert!(matches!(disposition, StartDisposition::Started { .. }));
        assert_eq!(supervisor.running_count(), 1);
        wait_for("the run to finish", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Succeeded,
                    ..
                }
            )
        });
        assert_eq!(supervisor.running_count(), 0);
    }

    #[test]
    fn a_non_zero_exit_is_a_failure_with_a_reason() {
        let binary = fake_agent("sup-fail", "exit 3");
        let (layout, agent) = layout("sup-fail");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        supervisor.start(&agent, Vec::new()).expect("started");
        wait_for("the failure to be recorded", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Failed,
                    ..
                }
            )
        });
        match supervisor.state("scribe") {
            State::Finished { reason, .. } => {
                assert!(reason.expect("a reason").contains("code 3"));
            }
            other => panic!("unexpected state {other:?}"),
        }
    }

    #[test]
    fn a_start_beyond_the_limit_waits_its_turn_and_then_runs() {
        let binary = fake_agent("sup-limit", "sleep 30");
        let (layout, _) = layout("sup-limit");
        let one = layout.ensure_agent("one").expect("one");
        let two = layout.ensure_agent("two").expect("two");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 1, Duration::from_secs(30));

        let first = supervisor.start(&one, Vec::new()).expect("started");
        assert!(matches!(first, StartDisposition::Started { .. }));
        let second = supervisor.start(&two, Vec::new()).expect("queued");
        match second {
            StartDisposition::Queued { position, .. } => assert_eq!(position, 1),
            other => panic!("expected a wait, got {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 1);
        assert_eq!(supervisor.waiting_count(), 1);
        assert!(matches!(
            supervisor.state("two"),
            State::Waiting { position: 1, .. }
        ));

        // With the slot free, the poll starts the start that was waiting.
        supervisor.stop("one");
        wait_for("the waiting start to begin", || {
            supervisor.poll(Instant::now());
            match supervisor.state("two") {
                State::Starting { .. } | State::Running { .. } => true,
                State::Finished { .. } => true,
                State::Waiting { .. } => false,
                State::Idle | State::Stopping { .. } => false,
            }
        });
        assert_eq!(supervisor.running_count(), 1);
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn calling_off_a_waiting_start_leaves_a_stopped_run_behind() {
        let binary = fake_agent("sup-cancel", "sleep 30");
        let (layout, _) = layout("sup-cancel");
        let one = layout.ensure_agent("one").expect("one");
        let two = layout.ensure_agent("two").expect("two");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 1, Duration::from_secs(30));
        supervisor.start(&one, Vec::new()).expect("started");
        supervisor.start(&two, Vec::new()).expect("queued");
        assert_eq!(supervisor.stop("two"), StopDisposition::Cancelled);
        assert_eq!(supervisor.waiting_count(), 0);
        assert!(matches!(
            supervisor.state("two"),
            State::Finished {
                outcome: RunOutcome::Stopped,
                ..
            }
        ));
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn a_run_that_overruns_its_timeout_is_stopped_and_reported() {
        let binary = fake_agent("sup-timeout", "sleep 30");
        let (layout, agent) = layout("sup-timeout");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_millis(200));
        supervisor.start(&agent, Vec::new()).expect("started");
        wait_for("the timeout", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::TimedOut,
                    ..
                }
            )
        });
        match supervisor.state("scribe") {
            State::Finished { reason, .. } => {
                assert!(reason.expect("a reason").contains("timeout"));
            }
            other => panic!("unexpected state {other:?}"),
        }
        assert_eq!(supervisor.running_count(), 0);
    }

    #[test]
    fn stopping_a_run_is_reported_as_stopped_and_the_process_is_gone() {
        let binary = fake_agent("sup-stop", "sleep 30");
        let (layout, agent) = layout("sup-stop");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        let disposition = supervisor.start(&agent, Vec::new()).expect("started");
        let pid = match disposition {
            StartDisposition::Started { pid, .. } => pid,
            other => panic!("unexpected {other:?}"),
        };
        wait_for("the process to be alive", || {
            supervisor.poll(Instant::now());
            matches!(supervisor.state("scribe"), State::Running { .. })
        });
        assert_eq!(supervisor.stop("scribe"), StopDisposition::Signalled);
        wait_for("the stop to be recorded", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Stopped,
                    ..
                }
            )
        });
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the child process must be gone"
        );
    }

    #[test]
    fn a_busy_agent_is_not_started_twice() {
        let binary = fake_agent("sup-busy", "sleep 30");
        let (layout, agent) = layout("sup-busy");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        supervisor.start(&agent, Vec::new()).expect("started");
        assert!(matches!(
            supervisor.start(&agent, Vec::new()).expect("refused"),
            StartDisposition::Busy { .. }
        ));
        supervisor.stop_all(Duration::from_secs(5));
    }

    #[test]
    fn the_terminal_record_of_a_stopped_run_is_written_once() {
        let binary = fake_agent("sup-record", "sleep 30");
        let (layout, agent) = layout("sup-record");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        let disposition = supervisor.start(&agent, Vec::new()).expect("started");
        let run_id = match disposition {
            StartDisposition::Started { run_id, .. } => run_id,
            other => panic!("unexpected {other:?}"),
        };
        wait_for("running", || {
            supervisor.poll(Instant::now());
            matches!(supervisor.state("scribe"), State::Running { .. })
        });
        supervisor.stop("scribe");
        wait_for("stopped", || {
            supervisor.poll(Instant::now());
            matches!(
                supervisor.state("scribe"),
                State::Finished {
                    outcome: RunOutcome::Stopped,
                    ..
                }
            )
        });
        let run_dir = supervisor.run_dir("scribe", &run_id);
        let file = transcript::read(&run_dir).expect("the transcript reads");
        let terminals = file
            .records
            .iter()
            .filter(|record| matches!(record, Record::RunFinished { .. }))
            .count();
        assert_eq!(terminals, 1, "exactly one terminal record");
        assert!(run_dir.join(PLAN_FILE).is_file());
        assert!(run_dir.join(LOG_FILE).is_file());
    }

    #[test]
    fn stop_all_changes_nothing_when_nothing_runs() {
        let binary = fake_agent("sup-empty", "exit 0");
        let (layout, _) = layout("sup-empty");
        let mut supervisor =
            Supervisor::new(layout.state_root(), binary, 10, Duration::from_secs(30));
        assert_eq!(supervisor.stop_all(Duration::from_millis(100)), 0);
    }
}
