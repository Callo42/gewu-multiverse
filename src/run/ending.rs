//! The owner's prepared/started run lifecycle. Constructors are private to this
//! module; the engine can only prepare, start, cancel, request an end and read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    FirstEnding, RunHandle, RunOutcome, RunningSession, prepare_started_dir,
    request_cancel_prepared, request_failed_finish, request_finish, request_stop, stop_uncertain,
};

/// The only way to create a run directory.
pub struct Owner;

/// The unforgeable authority for a terminal write. It has no public constructor;
/// only this module can make one.
pub struct EndingSeal {
    _private: (),
}

pub(super) fn seal() -> EndingSeal {
    EndingSeal { _private: () }
}

/// Test-only authority for integration tests that build a transcript by hand.
#[cfg(debug_assertions)]
pub fn seal_for_test() -> EndingSeal {
    EndingSeal { _private: () }
}

/// A prepared run that has not started a child.
pub struct PreparedRun {
    run_dir: PathBuf,
    run_id: String,
    workspace: PathBuf,
    armed: bool,
}

impl Owner {
    /// Prepare one run directory. This is the only owner constructor.
    pub fn prepare(run_dir: &Path, run_id: &str, workspace: &Path) -> Result<PreparedRun, String> {
        prepare_started_dir(run_dir)?;
        Ok(PreparedRun {
            run_dir: run_dir.to_path_buf(),
            run_id: run_id.to_owned(),
            workspace: workspace.to_path_buf(),
            armed: true,
        })
    }
}

impl PreparedRun {
    /// The prepared run's directory.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Start the child. On a spawn error the owner records the failed ending before
    /// returning the error; the guard is consumed either way.
    pub fn start<F>(mut self, spawn: F) -> Result<StartedRun, String>
    where
        F: FnOnce(&Path, &Path, &mut dyn FnMut(u32)) -> Result<Box<dyn RunningSession>, String>,
    {
        let mut spawned = |pid: u32| {
            // The engine records the pid in its own callback; this owner call is a
            // no-op, kept so the closure shape is explicit.
            let _ = pid;
        };
        // Keep `armed` true through the fallible call. A panic unwinds through Drop,
        // which records the prepared-run ending. Only the two normal results clear it.
        match spawn(&self.run_dir, &self.workspace, &mut spawned) {
            Ok(inner) => {
                self.armed = false;
                Ok(StartedRun {
                    handle: RunHandle { inner },
                    run_dir: self.run_dir.clone(),
                    run_id: self.run_id.clone(),
                })
            }
            Err(reason) => {
                let _ = request_failed_finish(&self.run_dir, &self.run_id, &reason);
                self.armed = false;
                Err(reason)
            }
        }
    }

    /// Cancel the prepared run; the guard is consumed.
    pub fn cancel(mut self, reason: &str) -> Result<(), String> {
        self.armed = false;
        request_cancel_prepared(&self.run_dir, &self.run_id, reason)
    }
}

impl Drop for PreparedRun {
    fn drop(&mut self) {
        if self.armed {
            let _ = request_cancel_prepared(
                &self.run_dir,
                &self.run_id,
                "the prepared run was dropped before it was spawned",
            );
        }
    }
}

/// A started child, owned by the owner. The handle and signal primitive are private.
pub struct StartedRun {
    handle: RunHandle,
    run_dir: PathBuf,
    run_id: String,
}

impl StartedRun {
    /// The run directory.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The child's pid / process-group id.
    pub fn pid(&self) -> u32 {
        self.handle.pid()
    }

    /// The child's exit status, when it has ended.
    pub fn try_status(&mut self) -> Option<std::process::ExitStatus> {
        self.handle.try_status()
    }

    /// Request the polite stop.
    pub fn request_stop(&mut self, proposed: FirstEnding) -> Result<FirstEnding, String> {
        request_stop(&mut self.handle, &self.run_dir, &self.run_id, proposed)
    }

    /// Request the force step after the polite grace.
    pub fn request_force(&mut self, reason: &str) -> Result<(), String> {
        super::request_force_stop(&mut self.handle, &self.run_dir, &self.run_id, reason)
    }

    /// Stop after an uncertainty, keeping the evidence.
    pub fn stop_uncertain(&mut self, error: &str, grace: Duration) -> String {
        stop_uncertain(&mut self.handle, &self.run_dir, &self.run_id, error, grace)
    }

    /// Request a stop and wait for the child before dropping the handle.
    pub fn request_stop_and_wait(
        &mut self,
        proposed: FirstEnding,
        reason: &str,
        grace: Duration,
    ) -> Result<(), String> {
        super::request_stop_and_wait(
            &mut self.handle,
            &self.run_dir,
            &self.run_id,
            proposed,
            reason,
            grace,
        )
    }

    /// Settle the one ending.
    pub fn request_finish(
        &mut self,
        outcome: RunOutcome,
        reason: Option<String>,
        grace: Duration,
        stop: Option<String>,
        first_ending: FirstEnding,
    ) -> Result<bool, String> {
        request_finish(
            &self.run_dir,
            self.handle.pid(),
            outcome,
            reason,
            grace,
            stop,
            first_ending,
        )
    }
}
