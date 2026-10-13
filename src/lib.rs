//! `gewu-multiverse` — a local, single-Universe Matrixverse.
//!
//! This crate is the terminal application: it opens the UI, reads one configuration
//! file, prepares one credentials file, records the first-run risk acknowledgment,
//! and runs agents as ordinary local child processes. A run reads an instruction, a
//! skill set and a declared list of shell steps, and leaves a log and an append-only
//! JSONL transcript. A run then calls the model at its own configured endpoint,
//! executes the tool calls the model asks for inside the agent's workspace, and stops
//! when the model finishes or a declared bound is reached. No request is made to the
//! Forgejo yet.
//!
//! The modules are:
//!
//! - [`cli`] — the arguments the program accepts;
//! - [`naming`] — what an agent name may be, because directories derive from it;
//! - [`paths`] — the two roots and every path derived from an agent's name;
//! - [`config`] — the configuration file and its validation;
//! - [`mode`] — the file modes the private files are held to;
//! - [`credentials`] — the credentials file, and the one secret it hands out;
//! - [`risk`] — the first-run risk acknowledgment and its record;
//! - [`time`] — the clock, and how an instant is written down;
//! - [`transcript`] — the append-only record of a run;
//! - [`run`] — a run's identity, its plan and its outcome;
//! - [`model`] — the direct model client, and its error taxonomy;
//! - [`agent_run`] — the agent process itself, one run at a time;
//! - [`supervisor`] — starting, watching and stopping agent processes;
//! - [`editor`] — handing a file to the user's own editor or pager;
//! - [`ui`] — drawing the screens;
//! - [`app`] — the state behind the screens and the key bindings.

#[cfg(not(unix))]
compile_error!(
    "gewu-multiverse targets Unix-like systems: it needs a terminal and 0600 file modes."
);

pub mod agent_run;
pub mod app;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod editor;
pub mod mode;
pub mod model;
pub mod naming;
pub mod paths;
pub mod risk;
pub mod run;
pub mod supervisor;
pub mod time;
pub mod transcript;
pub mod ui;

/// A tiny local HTTP server for the tests and demonstrations only; the application
/// never starts it and never listens on a socket.
#[doc(hidden)]
pub mod test_stub;

#[cfg(test)]
pub(crate) mod test_support;
