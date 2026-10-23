//! The engine's own settings file, `multiverse.toml`.
//!
//! It holds the product's small state: whether the terms were acknowledged, the one
//! model address every agent shares, the person's administrator login for the bundled
//! Forgejo, and each agent's name and started/stopped choice. It holds no secret; the
//! model key lives in the 0600 credentials file.
//!
//! A missing file is a fresh installation, not an error. A file that exists but cannot
//! be read is an error: starting with defaults over a broken file would hide the
//! person's agents and their choices.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::engine_api::Desired;
use crate::harness::{Program, Protocol};
use crate::mode::SECRET_FILE_MODE;
use crate::naming::is_safe_agent_name;

/// The file's name in the configuration root.
pub const FILE: &str = "multiverse.toml";

/// The most agents this product makes.
pub const MAX_AGENTS: usize = 20;

/// The default bound on model turns in one session.
pub const DEFAULT_MAX_TURNS: u32 = 24;

/// The whole file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The first-run terms were recorded.
    #[serde(default)]
    pub terms_acknowledged: bool,
    /// The wording of the terms that were recorded. A later build whose warnings
    /// changed re-shows them once, on its upgrade screen (F-06).
    #[serde(default)]
    pub terms_version: u32,
    /// The one model endpoint every agent uses.
    #[serde(default)]
    pub endpoint: EndpointSettings,
    /// The bundled Forgejo's person-facing facts.
    #[serde(default)]
    pub forgejo: ForgejoSettings,
    /// The agents, in the order they were made.
    #[serde(default)]
    pub agents: Vec<AgentSettings>,
    /// The next agent number this installation has never used. It only moves
    /// forward, so `+ add` after a removal makes a new agent and a new Forgejo
    /// account instead of reactivating the removed one (U5). An older file without
    /// it starts at the highest number its names and directories show.
    #[serde(default)]
    pub next_agent: usize,
    /// The program new agents get when the installation is fresh. `None` on an
    /// existing file means the built-in engine; a missing file is a fresh install
    /// and is loaded as `Some("pi")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<String>,
}

/// The shared endpoint. It holds no key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSettings {
    /// The completions URL as written (the legacy full address).
    #[serde(default)]
    pub url: String,
    /// The API base URL for a new-style connection, without `/chat/completions`.
    #[serde(default)]
    pub base_url: String,
    /// The connection protocol: `chat-completions`, `responses` or
    /// `anthropic-messages`.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// The model id.
    #[serde(default)]
    pub model: String,
    /// How many model turns one session may take (built-in only).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}

impl Default for EndpointSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            base_url: String::new(),
            protocol: default_protocol(),
            model: String::new(),
            max_turns: DEFAULT_MAX_TURNS,
        }
    }
}

impl EndpointSettings {
    /// Whether an address and a model are both set.
    pub fn configured(&self) -> bool {
        (!self.url.trim().is_empty() || !self.base_url.trim().is_empty())
            && !self.model.trim().is_empty()
    }

    /// The connection protocol, defaulting to Chat Completions for a legacy file.
    pub fn protocol(&self) -> Protocol {
        Protocol::parse(&self.protocol).unwrap_or(Protocol::ChatCompletions)
    }

    /// The API base URL. A legacy full `/chat/completions` address is accepted
    /// without rewriting the file.
    pub fn base_url(&self) -> String {
        let base = self.base_url.trim();
        if !base.is_empty() {
            return base.trim_end_matches('/').to_owned();
        }
        let url = self.url.trim().trim_end_matches('/');
        url.strip_suffix("/chat/completions")
            .unwrap_or(url)
            .to_owned()
    }

    /// The full Chat Completions address the built-in engine posts to.
    pub fn chat_url(&self) -> String {
        let url = self.url.trim();
        if !url.is_empty() {
            return url.to_owned();
        }
        format!("{}/chat/completions", self.base_url())
    }

    /// The turn bound actually used: a zero in a hand-written file is not a bound.
    pub fn turns(&self) -> u32 {
        self.max_turns.max(1)
    }
}

/// The person-facing facts about the bundled instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgejoSettings {
    /// The person's administrator login.
    #[serde(default)]
    pub admin_login: String,
    /// The newest repository change seen by any run of this installation. It
    /// survives a reopen so a destroyed repository is never traceless (W67-4).
    #[serde(default)]
    pub last_event: Option<String>,
}

/// One agent's saved facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    /// Its name, e.g. `agent-1`.
    #[serde(default)]
    pub name: String,
    /// `started` or `stopped`.
    #[serde(default = "default_desired")]
    pub desired: String,
    /// The native program id. Empty/absent is the built-in engine (legacy).
    #[serde(default)]
    pub harness: String,
    /// This agent's explicit connection, written at creation for a native program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,
    /// The Codex sandbox choice: empty/`native` is normal; `full-access` is the
    /// explicit D106 opt-in that runs Codex without its own sandbox inside the
    /// product's enforced Landlock.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codex_sandbox: String,
}

/// One agent's explicit provider connection. It holds no key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProvider {
    /// API base URL, without `/chat/completions`.
    #[serde(default)]
    pub base_url: String,
    /// `chat-completions`, `responses` or `anthropic-messages`.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// The model id.
    #[serde(default)]
    pub model: String,
}

impl AgentProvider {
    /// The protocol with the legacy default.
    pub fn protocol(&self) -> Protocol {
        Protocol::parse(&self.protocol).unwrap_or(Protocol::ChatCompletions)
    }

    /// Whether an address and model are both present.
    pub fn configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

fn default_protocol() -> String {
    "chat-completions".to_owned()
}

fn default_desired() -> String {
    "stopped".to_owned()
}

impl AgentSettings {
    /// The selected program, with an empty/unknown saved value read as built-in.
    pub fn harness(&self) -> Program {
        Program::parse(&self.harness).unwrap_or(Program::Builtin)
    }

    /// Set the selected program; built-in is written as the empty legacy value.
    pub fn set_harness(&mut self, program: Program) {
        self.harness = if program == Program::Builtin {
            String::new()
        } else {
            program.id().to_owned()
        };
    }

    /// The explicit provider for a native agent, if one was written.
    pub fn provider(&self) -> Option<&AgentProvider> {
        self.provider.as_ref()
    }

    /// The saved choice, read back as the contract's word.
    pub fn desired(&self) -> Desired {
        if self.desired == "started" {
            Desired::Started
        } else {
            Desired::Stopped
        }
    }

    /// Write the choice back as the file's word.
    pub fn set_desired(&mut self, desired: Desired) {
        self.desired = match desired {
            Desired::Started => "started",
            Desired::Stopped => "stopped",
        }
        .to_owned();
    }
}

impl Settings {
    /// Read the file, or the defaults when it is not there.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut settings: Self = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| format!("{} is not valid: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    default_harness: Some(Program::Pi.id().to_owned()),
                    ..Self::default()
                });
            }
            Err(error) => return Err(format!("{} cannot be read: {error}", path.display())),
        };
        // A file written before `next_agent` existed still must not reuse a name it
        // has ever held: the names in it are the floor.
        let names = settings.names();
        for name in names {
            settings.observe_agent_name(&name);
        }
        Ok(settings)
    }

    /// Write the file at 0600, atomically, so a reader never sees half a choice.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("{} cannot be created: {error}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| format!("the settings cannot be written as TOML: {error}"))?;
        let text = format!("# gewu-multiverse settings (written by the engine)\n{text}");
        crate::mode::write_private(path, text.as_bytes())
            .map_err(|error| format!("{} cannot be written: {error}", path.display()))
    }

    /// One agent's saved facts.
    pub fn agent(&self, name: &str) -> Option<&AgentSettings> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// One agent's saved facts, mutably.
    pub fn agent_mut(&mut self, name: &str) -> Option<&mut AgentSettings> {
        self.agents.iter_mut().find(|agent| agent.name == name)
    }

    /// Add an agent, refusing a name the paths cannot hold and one already present.
    pub fn add_agent(&mut self, name: &str, desired: Desired) -> Result<(), String> {
        if !is_safe_agent_name(name) {
            return Err(format!("`{name}` cannot name an agent"));
        }
        if self.agent(name).is_some() {
            return Ok(());
        }
        if self.agents.len() >= MAX_AGENTS {
            return Err(format!(
                "this installation already has the most agents it makes ({MAX_AGENTS})"
            ));
        }
        let mut agent = AgentSettings {
            name: name.to_owned(),
            desired: String::new(),
            harness: String::new(),
            provider: None,
            codex_sandbox: String::new(),
        };
        agent.set_desired(desired);
        self.agents.push(agent);
        self.observe_agent_name(name);
        Ok(())
    }

    /// Remove one agent's saved facts, keeping everything on disk.
    pub fn remove_agent(&mut self, name: &str) -> bool {
        let before = self.agents.len();
        self.agents.retain(|agent| agent.name != name);
        before != self.agents.len()
    }

    /// Note that this installation has held `agent-N`, so `N` is never given out
    /// again even after the agent is removed (U5).
    pub fn observe_agent_name(&mut self, name: &str) {
        if let Some(number) = agent_number(name) {
            self.next_agent = self.next_agent.max(number.saturating_add(1));
        }
    }

    /// The highest number any saved name has ever used, plus one.
    fn highest_number(&self) -> usize {
        self.agents
            .iter()
            .filter_map(|agent| agent_number(&agent.name))
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// The next name `+ add` will take, without changing anything.
    pub fn next_free_name(&self) -> String {
        format!(
            "agent-{}",
            self.next_agent.max(1).max(self.highest_number())
        )
    }

    /// Take the next never-used `agent-N`. The number only moves forward, so a
    /// removed agent's name, account and history are never silently reused.
    pub fn allocate_agent_name(&mut self) -> Result<String, String> {
        let number = self.next_agent.max(1).max(self.highest_number());
        if number > MAX_AGENTS {
            return Err(format!(
                "this installation has used all {MAX_AGENTS} agent numbers; the last agent cannot be replaced with a new one"
            ));
        }
        self.next_agent = number.saturating_add(1);
        Ok(format!("agent-{number}"))
    }

    /// Every name, in order.
    pub fn names(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.name.clone()).collect()
    }
}

/// The number in an `agent-N` name, when the name has that shape.
fn agent_number(name: &str) -> Option<usize> {
    name.strip_prefix("agent-")?.parse().ok()
}

/// The settings file below one configuration root.
pub fn path_in(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// A sentence for a settings write failure, kept to one line.
pub fn save_error(path: &Path, error: &str) -> String {
    format!(
        "the settings {} could not be saved: {error}",
        path.display()
    )
}

/// The mode a settings file is held to. It holds no secret, but it is the product's
/// own state and stays as private as the rest of its root.
pub const SETTINGS_MODE: u32 = SECRET_FILE_MODE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_a_fresh_installation() {
        let root = crate::test_support::scratch_root("settings-missing");
        let settings = Settings::load(&path_in(&root)).expect("a missing file is not an error");
        assert!(settings.agents.is_empty());
        assert!(!settings.terms_acknowledged);
        assert_eq!(settings.default_harness.as_deref(), Some("pi"));
    }

    #[test]
    fn the_started_choice_round_trips_through_the_file() {
        let root = crate::test_support::scratch_root("settings-round-trip");
        let path = path_in(&root);
        let mut settings = Settings::default();
        settings
            .add_agent("agent-1", Desired::Started)
            .expect("the agent is added");
        settings
            .add_agent("agent-2", Desired::Stopped)
            .expect("the agent is added");
        settings.save(&path).expect("the file is written");
        let read = Settings::load(&path).expect("the file is read");
        assert_eq!(read.agents.len(), 2);
        assert_eq!(
            read.agent("agent-1").expect("present").desired(),
            Desired::Started
        );
        assert_eq!(
            read.agent("agent-2").expect("present").desired(),
            Desired::Stopped
        );
        assert_eq!(read.next_free_name(), "agent-3");
    }

    #[test]
    fn an_existing_file_without_a_default_keeps_the_builtin_engine() {
        let root = crate::test_support::scratch_root("settings-legacy");
        let path = path_in(&root);
        let settings = Settings::default();
        settings.save(&path).expect("saved");
        let read = Settings::load(&path).expect("read");
        assert!(read.default_harness.is_none());
        assert!(read.agents.is_empty());
        let mut upgraded = read;
        upgraded
            .add_agent("scribe", Desired::Stopped)
            .expect("agent");
        assert_eq!(
            upgraded.agent("scribe").expect("present").harness(),
            Program::Builtin
        );
    }

    #[test]
    fn a_saved_agent_program_and_compatible_connection_round_trip() {
        let root = crate::test_support::scratch_root("settings-program");
        let path = path_in(&root);
        let mut settings = Settings::default();
        settings
            .add_agent("agent-1", Desired::Stopped)
            .expect("the agent");
        let agent = settings.agent_mut("agent-1").expect("present");
        agent.set_harness(Program::Codex);
        agent.provider = Some(AgentProvider {
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            protocol: "responses".to_owned(),
            model: "synthetic-model".to_owned(),
        });
        agent.codex_sandbox = "full-access".to_owned();
        settings.save(&path).expect("saved");
        let read = Settings::load(&path).expect("read");
        let agent = read.agent("agent-1").expect("present");
        assert_eq!(agent.harness(), Program::Codex);
        assert_eq!(
            agent.provider().expect("provider").protocol(),
            Protocol::Responses
        );
        assert_eq!(agent.codex_sandbox, "full-access");
    }

    #[test]
    fn an_unknown_field_is_refused_instead_of_silently_ignored() {
        let root = crate::test_support::scratch_root("settings-unknown");
        let path = path_in(&root);
        fs::write(&path, "terms_acknowledged = true\nsecret_thing = 1\n").expect("written");
        let error = Settings::load(&path).expect_err("an unknown field is refused");
        assert!(error.contains("not valid"), "{error}");
    }

    #[test]
    fn a_removal_keeps_the_other_agents_in_order() {
        let mut settings = Settings::default();
        settings
            .add_agent("agent-1", Desired::Stopped)
            .expect("added");
        settings
            .add_agent("agent-2", Desired::Stopped)
            .expect("added");
        assert!(settings.remove_agent("agent-1"));
        assert_eq!(settings.names(), vec!["agent-2"]);
        // U5: a removed number is never handed out again.
        assert_eq!(settings.next_free_name(), "agent-3");
        assert_eq!(
            settings.allocate_agent_name().expect("a fresh number"),
            "agent-3"
        );
    }
}
