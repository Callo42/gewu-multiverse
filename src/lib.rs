//! `gewu-multiverse` — one Forgejo on your machine, and the agents you run on it.
//!
//! Install the program and it brings its own Forgejo. The terminal board draws the
//! one Forgejo and the agents wired to it; each agent has its own Forgejo account,
//! one shared model and an instruction you write. Starting an agent keeps one run
//! after another going until you stop it. A run reads the instruction, calls the
//! model, uses the declared shell and Forgejo tools, and leaves a log and an
//! append-only JSONL transcript under the state root.
//!
//! The modules are:
//!
//! - [`cli`] — the arguments the program accepts;
//! - [`naming`] — what an agent name may be, because directories derive from it;
//! - [`paths`] — the two roots and every path derived from an agent's name;
//! - [`mode`] — the file modes the private files are held to;
//! - [`credentials`] — the credentials file, and the secrets it hands out;
//! - [`time`] — the clock, and how an instant is written down;
//! - [`transcript`] — the append-only record of a run;
//! - [`run`] — a run's identity, its plan and its outcome;
//! - [`model`] — the model client, and its error words;
//! - [`forgejo`] — the one Forgejo's client and its error taxonomy;
//! - [`forgejo_tools`] — the six Forgejo tools a run may declare, and their refusals;
//! - [`identity`] — one Forgejo account per agent, derived from its name;
//! - [`pull_gate`] — the open/update rules the Forgejo tools share;
//! - [`workspace`] — git work in an agent's workspace, under that identity;
//! - [`agent_run`] — the agent process itself, one run at a time;
//! - [`bundled_forgejo`] — the pinned Forgejo: download, instance, recovery;
//! - [`engine`] — the board's engine: Forgejo, accounts and the session loop;
//! - [`engine_api`] — the contract between the board and the engine;
//! - [`board`] — the picture and the keys;
//! - [`words`] — the product's plain-text sentences (counts with the right noun);
//! - [`editor`] — handing a file to the user's own editor or pager.
#[cfg(not(unix))]
compile_error!(
    "gewu-multiverse targets Unix-like systems: it needs a terminal and 0600 file modes."
);

pub mod agent_run;
pub mod board;
pub mod bundled_forgejo;
pub mod cli;
pub mod confinement;
pub mod credentials;
pub mod editor;
pub mod engine;
pub mod engine_api;
pub mod forgejo;
pub mod forgejo_tools;
mod git_proxy;
pub mod harness;
pub mod identity;
mod installer;
pub mod mcp_forgejo;
pub mod mode;
pub mod model;
pub mod naming;
pub mod paths;
pub mod pull_gate;
pub mod run;
pub mod time;
pub mod transcript;
pub mod words;
pub mod workspace;

/// A tiny local HTTP server for the tests and demonstrations only; the application
/// never starts it and never listens on a socket.
#[doc(hidden)]
pub mod test_stub;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) mod test_wait;
