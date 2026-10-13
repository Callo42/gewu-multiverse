//! `gewu-multiverse` — a local, single-Universe Matrixverse.
//!
//! This crate is the skeleton of the terminal application: it opens the UI, reads one
//! configuration file, prepares one credentials file, and records the first-run risk
//! acknowledgment. No agent runs yet and no network request is made yet.
//!
//! The modules are:
//!
//! - [`cli`] — the three arguments the program accepts;
//! - [`paths`] — the one directory the three files live in;
//! - [`config`] — the configuration file and its validation;
//! - [`mode`] — the file modes the two files are held to;
//! - [`credentials`] — the credentials file (created, never read);
//! - [`risk`] — the first-run risk acknowledgment and its record;
//! - [`ui`] — drawing the screens;
//! - [`app`] — the state behind the screens and the key bindings.

#[cfg(not(unix))]
compile_error!(
    "gewu-multiverse targets Unix-like systems: it needs a terminal and 0600 file modes."
);

pub mod app;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod mode;
pub mod paths;
pub mod risk;
pub mod ui;

#[cfg(test)]
pub(crate) mod test_support;
