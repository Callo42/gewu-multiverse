//! Where everything lives, and how an agent's directories are derived from its name.
//!
//! There are two roots.
//!
//! - The **configuration root** holds the files the skeleton already keeps there:
//!   `config.toml`, `credentials.toml`, the first-run acknowledgment, and — below
//!   `agents/` — one directory per agent, holding that agent's instruction, its
//!   skills and its workspace. On a Linux machine the root is
//!   `~/.config/gewu-multiverse`, or `$XDG_CONFIG_HOME/gewu-multiverse` when that is
//!   set. `--config-dir` moves it for one run.
//! - The **state root** holds run records: one directory per agent, then one
//!   directory per run. On a Linux machine it is `~/.local/state/gewu-multiverse`,
//!   or `$XDG_STATE_HOME/gewu-multiverse` when that is set. `--state-dir` moves it.
//!
//! Every per-agent path is one directory directly under `agents/` (or under the
//! state root), named by the agent's own name. Because a name is validated as a
//! single, safe path component ([`crate::naming`]), a name that cannot produce such a
//! directory is refused here with a sentence, and no caller ever has to guess.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::mode::{PRIVATE_DIR_MODE, hold_dir_private_below, set_mode};
use crate::naming::agent_name_problem;

/// Every path this program uses, derived from two root directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
    state_root: PathBuf,
}

/// Why a root directory could not be decided or prepared, or a name refused.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// No home directory and no `XDG_CONFIG_HOME`: there is nowhere to put the files.
    #[error(
        "no configuration directory could be found: set HOME or XDG_CONFIG_HOME, or pass --config-dir"
    )]
    NoConfigDir,
    /// The root directory could not be created.
    #[error("the directory {path} cannot be created: {source}")]
    Root {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// A directory could not be held to mode 0700.
    #[error("the directory {path} cannot be restricted to mode 0700: {source}")]
    RootMode {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The name cannot name a directory, so nothing for that agent can be derived.
    #[error(
        "the agent name `{name}` cannot name a directory: it {problem}; use lowercase letters, digits and single hyphens, at most 32 characters"
    )]
    UnsafeName {
        /// The name that was asked for.
        name: String,
        /// What is wrong with it.
        problem: String,
    },
}

impl Layout {
    /// Both roots under the user's own directories.
    pub fn in_user_config_dir() -> Result<Self, PathError> {
        let base = BaseDirs::new().ok_or(PathError::NoConfigDir)?;
        let root = base.config_dir().join("gewu-multiverse");
        // `state_dir` is absent on some systems; the data directory is the honest
        // fallback, and it is still the user's own directory.
        let state_root = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("gewu-multiverse");
        Ok(Self { root, state_root })
    }

    /// A configuration root the caller chose, which is what `--config-dir` and the
    /// tests pass. The state root follows it, so one moved directory is one whole
    /// installation; [`Layout::with_state_root`] separates them again.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let state_root = root.join("state");
        Self { root, state_root }
    }

    /// Put the run records somewhere else.
    pub fn with_state_root(mut self, state_root: impl Into<PathBuf>) -> Self {
        self.state_root = state_root.into();
        self
    }

    /// The configuration directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The run-record directory.
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// The one configuration file.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The one credentials file. Its contents are never read by the tool.
    pub fn credentials_file(&self) -> PathBuf {
        self.root.join("credentials.toml")
    }

    /// The record of the first-run risk acknowledgment.
    pub fn acknowledgment_file(&self) -> PathBuf {
        self.root.join("risk-acknowledgment.toml")
    }

    /// The directory holding one directory per agent.
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// Everything derived from one agent's name.
    pub fn agent(&self, name: &str) -> Result<AgentPaths, PathError> {
        AgentPaths::derive(self, name)
    }

    /// Create the root directory if it is not there yet, and hold it to mode 0700.
    pub fn ensure_root(&self) -> Result<(), PathError> {
        ensure_private_dir(&self.root)
    }

    /// Create the directory holding one directory per agent, and hold it to 0700.
    pub fn ensure_agents_dir(&self) -> Result<(), PathError> {
        ensure_private_dir_below(&self.root, &self.agents_dir())
    }

    /// Create whatever a run of one agent needs before it starts: the agent's own
    /// configuration directory, its skills directory and its workspace.
    pub fn ensure_agent(&self, name: &str) -> Result<AgentPaths, PathError> {
        self.ensure_root()?;
        self.ensure_agents_dir()?;
        let agent = self.agent(name)?;
        agent.ensure()?;
        Ok(agent)
    }
}

/// The directories of one agent, all derived from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    name: String,
    root: PathBuf,
    state_root: PathBuf,
    config_dir: PathBuf,
    instruction: PathBuf,
    skills_dir: PathBuf,
    steps_file: PathBuf,
    workspace: PathBuf,
    state_dir: PathBuf,
}

impl AgentPaths {
    /// Derive every directory of one agent from its name.
    ///
    /// An unsafe name is refused here, before any path exists, so no caller can
    /// accidentally build a path from a name that did not pass the rule.
    pub fn derive(layout: &Layout, name: &str) -> Result<Self, PathError> {
        if let Some(problem) = agent_name_problem(name) {
            return Err(PathError::UnsafeName {
                name: name.to_owned(),
                problem,
            });
        }
        let config_dir = layout.agents_dir().join(name);
        Ok(Self {
            name: name.to_owned(),
            root: layout.root().to_path_buf(),
            state_root: layout.state_root().to_path_buf(),
            instruction: config_dir.join("instruction.md"),
            skills_dir: config_dir.join("skills"),
            steps_file: config_dir.join("steps.txt"),
            workspace: config_dir.join("workspace"),
            state_dir: layout.state_root().join(name),
            config_dir,
        })
    }

    /// The same agent, reading the instruction document the configuration names.
    ///
    /// The instruction path is relative to the configuration directory and is checked
    /// when the configuration is read (no absolute path, no `..`), so joining it to
    /// the root can only reach inside the configuration directory. The common case —
    /// the form writes `agents/<name>/instruction.md` — is the derived default, but a
    /// hand-written configuration may point the field elsewhere, and then every screen
    /// and the run must read that file (P-3).
    pub fn with_instruction(mut self, instruction: PathBuf) -> Self {
        self.instruction = instruction;
        self
    }

    /// The agent's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The configuration root this agent lives under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The state root this agent's run records live under.
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// The agent's configuration directory: its instruction, skills and workspace.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The instruction document a run reads.
    pub fn instruction(&self) -> &Path {
        &self.instruction
    }

    /// The directory holding one directory per skill.
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// The declared shell steps a run executes, one per line.
    pub fn steps_file(&self) -> &Path {
        &self.steps_file
    }

    /// The agent's own workspace. A layout, not a sandbox.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Where this agent's run records are kept.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The record directory of one run of this agent.
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.state_dir.join(run_id)
    }

    /// Create the configuration directory, the skills directory and the workspace.
    ///
    /// A configured instruction may live in a directory of its own (for example
    /// `instructions/scribe.md`), so the instruction's parent is created too: the
    /// editor writes the file there, and `e` must not fail because the directory the
    /// configuration named does not exist yet. Every directory on the way, from the
    /// configuration root down, is held to 0700 (F-1).
    pub fn ensure(&self) -> Result<(), PathError> {
        ensure_private_dir_below(&self.root, &self.config_dir)?;
        ensure_private_dir_below(&self.root, &self.skills_dir)?;
        ensure_private_dir_below(&self.root, &self.workspace)?;
        if let Some(parent) = self.instruction.parent() {
            ensure_private_dir_below(&self.root, parent)?;
        }
        Ok(())
    }

    /// Create this agent's run-record directory, holding every directory from the
    /// state root down to it to 0700 (F-1).
    pub fn ensure_state_dir(&self) -> Result<(), PathError> {
        ensure_private_dir_below(&self.state_root, &self.state_dir)
    }

    /// Create one run's record directory, holding every directory from the state
    /// root down to it to 0700 (F-1).
    pub fn ensure_run_dir(&self, run_id: &str) -> Result<PathBuf, PathError> {
        let run_dir = self.run_dir(run_id);
        ensure_private_dir_below(&self.state_root, &run_dir)?;
        Ok(run_dir)
    }
}

/// Create a directory if it is not there yet, and hold it to mode 0700.
fn ensure_private_dir(path: &Path) -> Result<(), PathError> {
    fs::create_dir_all(path).map_err(|source| PathError::Root {
        path: path.to_path_buf(),
        source,
    })?;
    set_mode(path, PRIVATE_DIR_MODE).map_err(|source| PathError::RootMode {
        path: path.to_path_buf(),
        source,
    })
}

/// Create a directory if it is not there yet, and hold it, and every directory
/// between `stop` and it, to mode 0700.
fn ensure_private_dir_below(stop: &Path, path: &Path) -> Result<(), PathError> {
    fs::create_dir_all(path).map_err(|source| PathError::Root {
        path: path.to_path_buf(),
        source,
    })?;
    hold_dir_private_below(stop, path).map_err(|source| PathError::RootMode {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_three_files_sit_in_the_root() {
        let layout = Layout::rooted_at("/tmp/example-root");
        assert_eq!(
            layout.config_file(),
            Path::new("/tmp/example-root/config.toml")
        );
        assert_eq!(
            layout.credentials_file(),
            Path::new("/tmp/example-root/credentials.toml")
        );
        assert_eq!(
            layout.acknowledgment_file(),
            Path::new("/tmp/example-root/risk-acknowledgment.toml")
        );
    }

    #[test]
    fn ensure_root_creates_the_directory_once_and_keeps_it_private() {
        let root = scratch_root("layout");
        let layout = Layout::rooted_at(root.join("nested"));
        layout.ensure_root().expect("the root is created");
        layout.ensure_root().expect("a second call is harmless");
        assert!(layout.root().is_dir());
        let mode = fs::metadata(layout.root())
            .expect("the root can be described")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_DIR_MODE);
    }

    #[test]
    fn every_directory_of_an_agent_is_derived_from_its_name() {
        let layout = Layout::rooted_at("/tmp/example-root").with_state_root("/tmp/example-state");
        let agent = layout.agent("scribe").expect("a safe name is accepted");
        assert_eq!(
            agent.config_dir(),
            Path::new("/tmp/example-root/agents/scribe")
        );
        assert_eq!(
            agent.instruction(),
            Path::new("/tmp/example-root/agents/scribe/instruction.md")
        );
        assert_eq!(
            agent.skills_dir(),
            Path::new("/tmp/example-root/agents/scribe/skills")
        );
        assert_eq!(
            agent.steps_file(),
            Path::new("/tmp/example-root/agents/scribe/steps.txt")
        );
        assert_eq!(
            agent.workspace(),
            Path::new("/tmp/example-root/agents/scribe/workspace")
        );
        assert_eq!(agent.state_dir(), Path::new("/tmp/example-state/scribe"));
        assert_eq!(
            agent.run_dir("run-1"),
            Path::new("/tmp/example-state/scribe/run-1")
        );
    }

    #[test]
    fn a_name_that_cannot_name_a_directory_is_refused_with_a_sentence() {
        let layout = Layout::rooted_at("/tmp/example-root");
        for name in ["", "..", "a/b", "Upper", "a--b", "-a"] {
            let error = layout.agent(name).expect_err("the name is refused");
            let message = error.to_string();
            assert!(message.contains("cannot name a directory"), "{message}");
            assert!(message.contains(name) || name.is_empty(), "{message}");
        }
    }

    #[test]
    fn distinct_names_never_share_a_workspace() {
        let layout = Layout::rooted_at("/tmp/example-root");
        let mut seen: Vec<PathBuf> = Vec::new();
        for name in ["a", "b", "agent-1", "agent-2", "scribe", "checker"] {
            let workspace = layout
                .agent(name)
                .expect("a safe name is accepted")
                .workspace()
                .to_path_buf();
            assert!(!seen.contains(&workspace), "{workspace:?} is used twice");
            seen.push(workspace);
        }
    }

    #[test]
    fn one_agent_never_sits_inside_another() {
        // Because a name is one path component with no separator, no agent's
        // directory can contain another agent's directory.
        let layout = Layout::rooted_at("/tmp/example-root");
        let scribe = layout.agent("scribe").expect("accepted");
        let scribe2 = layout.agent("scribe2").expect("accepted");
        assert!(!scribe2.workspace().starts_with(scribe.workspace()));
        assert!(!scribe.workspace().starts_with(scribe2.workspace()));
    }

    #[test]
    fn ensuring_an_agent_creates_only_its_own_private_directories() {
        let root = scratch_root("ensure-agent");
        let layout = Layout::rooted_at(root.clone());
        let agent = layout
            .ensure_agent("scribe")
            .expect("the agent is prepared");
        // F-1: the intermediate `agents/` directory is the product's too, and must
        // carry the same 0700 as the directories inside it.
        for dir in [
            layout.agents_dir(),
            agent.config_dir().to_path_buf(),
            agent.skills_dir().to_path_buf(),
            agent.workspace().to_path_buf(),
        ] {
            assert!(dir.is_dir(), "{dir:?} must exist");
            let mode = fs::metadata(&dir)
                .expect("the directory can be described")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, PRIVATE_DIR_MODE, "{dir:?}");
        }
        assert!(!agent.instruction().exists(), "no instruction is invented");
        assert!(!agent.state_dir().exists(), "no run record is created yet");
    }

    #[test]
    fn every_state_directory_the_product_makes_is_private() {
        // F-1: the state root, the per-agent state directory and the per-run record
        // directory were left at the umask default while the agent's configuration
        // directory was 0700.
        let root = scratch_root("ensure-state");
        let layout = Layout::rooted_at(root.clone());
        let agent = layout
            .ensure_agent("scribe")
            .expect("the agent is prepared");
        agent.ensure_state_dir().expect("the state directory is made");
        let run_dir = agent
            .ensure_run_dir("run-1")
            .expect("the run directory is made");
        for dir in [
            layout.state_root(),
            agent.state_dir(),
            run_dir.as_path(),
        ] {
            assert!(dir.is_dir(), "{dir:?} must exist");
            let mode = fs::metadata(dir)
                .expect("the directory can be described")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, PRIVATE_DIR_MODE, "{dir:?}");
        }
    }
}
