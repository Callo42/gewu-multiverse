//! How one session is prepared and started.
//!
//! The engine decides *when* a session runs; this module does the two slow things
//! around it. `prepare` clones or fetches every repository of `team` as the agent and
//! writes the run's plan and non-secret Forgejo context. `spawn` starts the existing
//! `--run-agent` run in its own process group and returns a handle the engine polls.
//!
//! The split matters to the engine's locking: the slow work runs without the engine's
//! state lock, and only the short `spawn` happens while the lock is held, so
//! `shutdown` can always see every child the engine started.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use crate::run::{NativePlan, RunForgejo, RunForgejoRepository, RunPlan, write_forgejo_context};
use crate::workspace::{self, GitCredential, GitIdentity, Prepared};

/// One repository a session may use, as the run's context records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRepository {
    /// `owner/name`.
    pub full_name: String,
    /// The directory under the workspace.
    pub directory: String,
}

/// Everything one session needs. It holds no token: the run reads the token from the
/// credentials file at the moment a tool asks for it, and git asks through the
/// askpass pipe.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// The agent's name.
    pub agent: String,
    /// A fresh run id.
    pub run_id: String,
    /// The run's record directory.
    pub run_dir: PathBuf,
    /// The root the run's record directory sits under. It is held to 0700 with the
    /// agent's state directory before a run writes anything.
    pub state_root: PathBuf,
    /// The agent's workspace.
    pub workspace: PathBuf,
    /// The instruction the run reads.
    pub instruction: PathBuf,
    /// The skills directory.
    pub skills_dir: PathBuf,
    /// The shared endpoint.
    pub endpoint: String,
    /// The shared model id.
    pub model: String,
    /// The turn bound.
    pub max_turns: u32,
    /// The session's timeout, written into the plan.
    pub timeout_secs: u64,
    /// The credentials file.
    pub credentials: PathBuf,
    /// The agent's Forgejo login.
    pub login: String,
    /// The agent's Forgejo email.
    pub email: String,
    /// The bundled instance's URL.
    pub forgejo_url: String,
    /// The repositories of `team` this session may use.
    pub repositories: Vec<SessionRepository>,
    /// How the run's shell is confined.
    pub confinement: crate::confinement::Method,
    /// The directories the confined shell must not see.
    pub hide: Vec<PathBuf>,
    /// The native program this run launches, when it is not the built-in loop.
    pub native: Option<NativeRequest>,
}

/// The immutable native program facts carried from the agent's settings into a run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRequest {
    /// Program id.
    pub harness: String,
    /// Actual installed version.
    pub version: String,
    /// Executable.
    pub program: PathBuf,
    /// Product-owned install root.
    pub install_root: PathBuf,
    /// Connection protocol.
    pub protocol: String,
    /// API base URL.
    pub base_url: String,
    /// Model id.
    pub model: String,
    /// Explicit Codex sandbox choice (`native` or `full-access`).
    #[serde(default)]
    pub codex_sandbox: String,
}

/// A session whose slow work is done and whose plan is on disk.
pub struct PreparedSession {
    /// The run's record directory.
    pub run_dir: PathBuf,
    /// The agent's workspace.
    pub workspace: PathBuf,
    /// Owner guard for the prepared run; consumed by the owner's start/cancel/drop.
    pub guard: crate::run::PreparedRun,
}

/// The name of the pid file inside a run's record directory.
///
/// It is written as soon as the child exists and removed when it has been reaped, so
/// a later start can find a session that a killed program left behind. The file holds
/// nothing but the number.
pub const PID_FILE: &str = "session.pid";

/// The run's process-group record, written beside its pid. The group id outlives
/// the supervisor that leads it, so a later start can stop a shell whose supervisor
/// was killed (F-02/F-03).
pub const PGID_FILE: &str = "session.pgid";

/// A running child, as the engine polls it.
pub trait RunningSession: Send {
    /// The child's pid, which is also its process-group id.
    fn pid(&self) -> u32;
    /// The exit status when it has ended, else `None`.
    fn try_status(&mut self) -> Option<ExitStatus>;
    /// Ask the process group to stop; `force` means `SIGKILL`.
    fn signal(&mut self, force: bool);
}

/// Prepares and starts sessions.
pub trait SessionSpawner: Send + Sync {
    /// Clone/fetch the repositories and write the run's plan. This is the slow half.
    fn prepare(&self, request: &SessionRequest) -> Result<PreparedSession, String>;
    /// Start the prepared run and report the child's pid through `on_spawn` as soon
    /// as the child exists, before any wait of the spawner's own. The engine records
    /// that pid so `shutdown` can signal a spawn that has not returned yet.
    fn spawn_with(
        &self,
        run_dir: &Path,
        workspace: &Path,
        on_spawn: &mut dyn FnMut(u32),
    ) -> Result<Box<dyn RunningSession>, String>;
    /// Start the prepared run when the caller does not need the pid early.
    fn spawn(&self, prepared: &PreparedSession) -> Result<Box<dyn RunningSession>, String> {
        self.spawn_with(&prepared.run_dir, &prepared.workspace, &mut |_pid| {})
    }
}

/// The real spawner: git in the agent's workspace and the product's own run binary.
pub struct RealSpawner {
    binary: PathBuf,
}

impl RealSpawner {
    /// Spawn runs with `binary`, which is the product executable.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl SessionSpawner for RealSpawner {
    fn prepare(&self, request: &SessionRequest) -> Result<PreparedSession, String> {
        let guard =
            crate::run::Owner::prepare(&request.run_dir, &request.run_id, &request.workspace)?;
        crate::mode::hold_dir_private_below(&request.state_root, &request.run_dir).map_err(
            |error| {
                format!(
                    "{} cannot be restricted: {error}",
                    request.run_dir.display()
                )
            },
        )?;
        let identity = GitIdentity {
            login: request.login.clone(),
            email: request.email.clone(),
        };
        let credential = GitCredential {
            askpass: &self.binary,
            credentials: &request.credentials,
            agent: &request.agent,
            login: &request.login,
        };
        for repository in &request.repositories {
            let destination = request.workspace.join(&repository.directory);
            match workspace::prepare(
                &request.forgejo_url,
                &repository.full_name,
                &request.workspace,
                &destination,
                &identity,
                Some(&credential),
            ) {
                Ok((_, Prepared::AlreadyPresent)) => {
                    workspace::fetch(&destination, &credential).map_err(|error| {
                        format!(
                            "`{}` could not be fetched before the session: {}",
                            repository.full_name,
                            crate::model::excerpt(&error.detail(), 300)
                        )
                    })?;
                }
                Ok((_, Prepared::Cloned)) => {}
                Err(error) => {
                    return Err(format!(
                        "`{}` could not be cloned before the session: {}",
                        repository.full_name,
                        crate::model::excerpt(&error.detail(), 300)
                    ));
                }
            }
        }
        let context = RunForgejo {
            url: request.forgejo_url.clone(),
            login: request.login.clone(),
            repositories: request
                .repositories
                .iter()
                .map(|repository| RunForgejoRepository {
                    full_name: repository.full_name.clone(),
                    directory: repository.directory.clone(),
                })
                .collect(),
        };
        let plan = RunPlan {
            run_id: request.run_id.clone(),
            agent: request.agent.clone(),
            workspace: request.workspace.clone(),
            instruction: request.instruction.clone(),
            skills_dir: request.skills_dir.clone(),
            endpoint: request.endpoint.clone(),
            model: request.model.clone(),
            credentials: request.credentials.clone(),
            max_turns: request.max_turns,
            steps: Vec::new(),
            timeout_secs: request.timeout_secs,
            confinement: request.confinement.as_str().to_owned(),
            hide: request.hide.clone(),
        };
        plan.write(&request.run_dir)
            .map_err(|error| format!("the session's plan cannot be written: {error}"))?;
        if !context.repositories.is_empty() {
            crate::run::write_forgejo_context(&request.run_dir, &context).map_err(|error| {
                format!("the session's Forgejo context cannot be written: {error}")
            })?;
        }
        if let Some(native) = request.native.as_ref() {
            let plan = NativePlan {
                harness: native.harness.clone(),
                version: native.version.clone(),
                program: native.program.clone(),
                install_root: native.install_root.clone(),
                protocol: native.protocol.clone(),
                base_url: native.base_url.clone(),
                model: native.model.clone(),
                codex_sandbox: native.codex_sandbox.clone(),
            };
            plan.write(&request.run_dir).map_err(|error| {
                format!("the session's native program plan cannot be written: {error}")
            })?;
        }
        Ok(PreparedSession {
            run_dir: request.run_dir.clone(),
            workspace: request.workspace.clone(),
            guard,
        })
    }

    fn spawn_with(
        &self,
        run_dir: &Path,
        workspace: &Path,
        on_spawn: &mut dyn FnMut(u32),
    ) -> Result<Box<dyn RunningSession>, String> {
        // The run process asks the kernel to signal it when its *parent thread* goes
        // away (`PR_SET_PDEATHSIG`). A short-lived thread would therefore kill
        // every session as soon as it returned, so a dedicated host thread does the
        // spawn and then waits for the child. That thread is the parent for the whole
        // life of the run, and the engine talks to the child through a channel.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let binary = self.binary.clone();
        let run_dir = run_dir.to_path_buf();
        let workspace = workspace.to_path_buf();
        std::thread::spawn(move || {
            let child = Command::new(&binary)
                .arg("--run-agent")
                .arg(&run_dir)
                .current_dir(&workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn();
            match child {
                Ok(mut child) => {
                    let pid = child.id();
                    // The record is written before the engine is told the pid, so a
                    // kill between these two lines still leaves a trace to recover.
                    let recorded =
                        write_pid(&run_dir, pid).and_then(|()| write_pgid(&run_dir, pid));
                    if let Err(reason) = recorded {
                        crate::run::adapter_signal_run_group(pid, true);
                        let _ = child.wait();
                        let _ = ready_tx.send(Err(reason));
                        return;
                    }
                    let _ = ready_tx.send(Ok(pid));
                    let status = child.wait();
                    // The pid/group records are identity handles, not transient
                    // bookkeeping. They stay until the engine has written a durable
                    // ending and verified cleanup (`run::settle_ending`), or until a
                    // later open does the same. Removing them here lost the only
                    // recovery handle of a run whose supervisor wrote no shell
                    // record, even though its ending had not been recorded yet.
                    let _ = status_tx.send(status);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "the session process could not be started: {error}"
                    )));
                }
            }
        });
        match ready_rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Ok(pid)) => {
                // The child exists and its pid record is written; report it before
                // this function returns so a shutdown can signal it during any wait.
                on_spawn(pid);
                Ok(Box::new(HostedSession {
                    pid,
                    status: status_rx,
                    exited: None,
                }))
            }
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err("the session process did not start within 30 s".to_owned()),
        }
    }
}

/// A child whose long-lived host thread is its parent and reaps it.
struct HostedSession {
    pid: u32,
    status: Receiver<std::io::Result<ExitStatus>>,
    exited: Option<ExitStatus>,
}

impl RunningSession for HostedSession {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_status(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.exited {
            return Some(status);
        }
        match self.status.try_recv() {
            Ok(Ok(status)) => {
                self.exited = Some(status);
                Some(status)
            }
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                let status = ExitStatus::from_raw(1 << 8);
                self.exited = Some(status);
                Some(status)
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    fn signal(&mut self, force: bool) {
        crate::run::adapter_signal_run_group(self.pid, force);
    }
}

impl Drop for HostedSession {
    fn drop(&mut self) {
        if self.exited.is_none() {
            crate::run::adapter_signal_run_group(self.pid, true);
        }
    }
}

/// The tests' spawner: it prepares nothing and starts a handle the test completes.
#[derive(Debug, Default)]
pub struct FakeSpawner {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug, Default)]
struct FakeInner {
    next_pid: u32,
    live: Vec<u32>,
    exits: HashMap<u32, i32>,
    starts: Vec<String>,
    prepares: Vec<String>,
    last_prepared: Option<String>,
    last_run_dir: Option<PathBuf>,
    last_repositories: Vec<SessionRepository>,
    prepare_failure: Option<String>,
    prepare_failure_after_dir: Option<String>,
    spawn_failure: Option<String>,
    spawn_failure_after_block: Option<String>,
    block_next_spawn: Option<(std::sync::mpsc::Sender<u32>, std::sync::mpsc::Receiver<()>)>,
    block_next_prepare: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

impl FakeSpawner {
    /// A fresh fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// Finish the newest live session with an exit code.
    pub fn finish_current(&self, code: i32) {
        let mut inner = self.inner.lock().expect("the fake is not poisoned");
        if let Some(pid) = inner.live.last().copied() {
            inner.exits.insert(pid, code);
        }
    }

    /// Finish every live session with an exit code.
    pub fn finish_all(&self, code: i32) {
        let mut inner = self.inner.lock().expect("the fake is not poisoned");
        for pid in inner.live.clone() {
            inner.exits.insert(pid, code);
        }
    }

    /// How many sessions were prepared and how many were started.
    pub fn counts(&self) -> (usize, usize) {
        let inner = self.inner.lock().expect("the fake is not poisoned");
        (inner.prepares.len(), inner.starts.len())
    }

    /// The last start's agent name, when one started.
    pub fn last_started(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .starts
            .last()
            .cloned()
    }

    /// The run directory of the newest preparation, when one happened.
    pub fn last_run_dir(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .last_run_dir
            .clone()
    }

    /// The repositories the newest preparation was given.
    pub fn last_repositories(&self) -> Vec<SessionRepository> {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .last_repositories
            .clone()
    }

    /// Make the next `prepare` fail with a sentence.
    pub fn fail_prepare(&self, reason: &str) {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .prepare_failure = Some(reason.to_owned());
    }

    /// Make the next `prepare` fail after it created the run directory.
    pub fn fail_prepare_after_dir(&self, reason: &str) {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .prepare_failure_after_dir = Some(reason.to_owned());
    }

    /// Make the next spawn report a pid, wait for the release, then fail.
    pub fn fail_spawn_after_block(&self, reason: &str) {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .spawn_failure_after_block = Some(reason.to_owned());
    }

    /// Make the next spawn fail before a child exists.
    pub fn fail_spawn(&self, reason: &str) {
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .spawn_failure = Some(reason.to_owned());
    }

    /// Make the next prepare report that the run directory exists and then wait for
    /// a release before returning. Returns the start report and release sender.
    pub fn block_next_prepare(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .block_next_prepare = Some((started_tx, release_rx));
        (started_rx, release_tx)
    }

    /// Make the next spawn report its pid and then wait for a release before it
    /// returns. Returns the pid report and the release sender.
    pub fn block_next_spawn(
        &self,
    ) -> (std::sync::mpsc::Receiver<u32>, std::sync::mpsc::Sender<()>) {
        let (pid_tx, pid_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        self.inner
            .lock()
            .expect("the fake is not poisoned")
            .block_next_spawn = Some((pid_tx, release_rx));
        (pid_rx, release_tx)
    }

    /// Whether a started pid has not been signalled to an end.
    pub fn is_live(&self, pid: u32) -> bool {
        let inner = self.inner.lock().expect("the fake is not poisoned");
        inner.live.contains(&pid) && !inner.exits.contains_key(&pid)
    }

    /// Every started pid that has not been signalled to an end.
    pub fn live(&self) -> Vec<u32> {
        let inner = self.inner.lock().expect("the fake is not poisoned");
        inner
            .live
            .iter()
            .copied()
            .filter(|pid| !inner.exits.contains_key(pid))
            .collect()
    }
}

impl SessionSpawner for FakeSpawner {
    fn prepare(&self, request: &SessionRequest) -> Result<PreparedSession, String> {
        let mut inner = self.inner.lock().expect("the fake is not poisoned");
        if let Some(reason) = inner.prepare_failure.take() {
            return Err(reason);
        }
        let guard =
            crate::run::Owner::prepare(&request.run_dir, &request.run_id, &request.workspace)?;
        let plan = RunPlan {
            run_id: request.run_id.clone(),
            agent: request.agent.clone(),
            workspace: request.workspace.clone(),
            instruction: request.instruction.clone(),
            skills_dir: request.skills_dir.clone(),
            endpoint: request.endpoint.clone(),
            model: request.model.clone(),
            credentials: request.credentials.clone(),
            max_turns: request.max_turns,
            steps: Vec::new(),
            timeout_secs: request.timeout_secs,
            confinement: request.confinement.as_str().to_owned(),
            hide: request.hide.clone(),
        };
        plan.write(&request.run_dir)
            .map_err(|error| format!("the session's plan cannot be written: {error}"))?;
        if !request.repositories.is_empty() {
            let context = RunForgejo {
                url: request.forgejo_url.clone(),
                login: request.login.clone(),
                repositories: request
                    .repositories
                    .iter()
                    .map(|repository| RunForgejoRepository {
                        full_name: repository.full_name.clone(),
                        directory: repository.directory.clone(),
                    })
                    .collect(),
            };
            write_forgejo_context(&request.run_dir, &context).map_err(|error| {
                format!("the session's Forgejo context cannot be written: {error}")
            })?;
        }
        inner.prepares.push(request.agent.clone());
        inner.last_prepared = Some(request.agent.clone());
        inner.last_run_dir = Some(request.run_dir.clone());
        inner.last_repositories = request.repositories.clone();
        let after_dir_failure = inner.prepare_failure_after_dir.take();
        if let Some((started, release)) = inner.block_next_prepare.take() {
            drop(inner);
            let _ = started.send(());
            let _ = release.recv();
            if let Some(reason) = after_dir_failure {
                return Err(reason);
            }
        } else if let Some(reason) = after_dir_failure {
            drop(inner);
            return Err(reason);
        }
        Ok(PreparedSession {
            run_dir: request.run_dir.clone(),
            workspace: request.workspace.clone(),
            guard,
        })
    }

    fn spawn_with(
        &self,
        _run_dir: &Path,
        _workspace: &Path,
        on_spawn: &mut dyn FnMut(u32),
    ) -> Result<Box<dyn RunningSession>, String> {
        let (pid, block, after_block_failure) = {
            let mut inner = self.inner.lock().expect("the fake is not poisoned");
            if let Some(reason) = inner.spawn_failure.take() {
                return Err(reason);
            }
            inner.next_pid += 1;
            let pid = 40_000 + inner.next_pid;
            inner.live.push(pid);
            let agent = inner
                .last_prepared
                .clone()
                .unwrap_or_else(|| "fake".to_owned());
            inner.starts.push(agent);
            (
                pid,
                inner.block_next_spawn.take(),
                inner.spawn_failure_after_block.take(),
            )
        };
        // The pid is reported before the optional wait: a shutdown during the wait
        // must be able to name the child the fake is holding.
        on_spawn(pid);
        if let Some((reported, release)) = block {
            let _ = reported.send(pid);
            let _ = release.recv();
        }
        if let Some(reason) = after_block_failure {
            return Err(reason);
        }
        Ok(Box::new(FakeHandle {
            inner: Arc::clone(&self.inner),
            pid,
        }))
    }
}

struct FakeHandle {
    inner: Arc<Mutex<FakeInner>>,
    pid: u32,
}

impl RunningSession for FakeHandle {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_status(&mut self) -> Option<ExitStatus> {
        let inner = self.inner.lock().expect("the fake is not poisoned");
        inner
            .exits
            .get(&self.pid)
            .map(|code| ExitStatus::from_raw(code << 8))
    }

    fn signal(&mut self, _force: bool) {
        let mut inner = self.inner.lock().expect("the fake is not poisoned");
        inner.exits.entry(self.pid).or_insert(0);
    }
}

/// Write one run's pid file at 0600.
fn write_pid(run_dir: &Path, pid: u32) -> Result<(), String> {
    crate::mode::write_private(run_dir.join(PID_FILE), pid.to_string().as_bytes())
        .map_err(|error| format!("the session's pid record cannot be written: {error}"))
}

/// Write one run's process-group id at 0600.
fn write_pgid(run_dir: &Path, pgid: u32) -> Result<(), String> {
    crate::mode::write_private(run_dir.join(PGID_FILE), pgid.to_string().as_bytes())
        .map_err(|error| format!("the session's process-group record cannot be written: {error}"))
}

/// The exit code of a status, when it has one; a signal is reported as 128+signal.
pub fn exit_code(status: ExitStatus) -> Option<i32> {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
}
