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

use std::collections::BTreeMap;
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

/// Default idle timeout (seconds) before a per-role resident harness runtime is parked.
pub const DEFAULT_RUNTIME_IDLE_SECS: u64 = 300;

/// Default fallback interval (seconds) for `hybrid` wake mode.
pub const DEFAULT_HYBRID_INTERVAL_SECS: u64 = 300;

/// How a started role is woken between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WakeMode {
    /// Legacy/explicit timer polling (`NO_WORK_DELAY` or `wake_interval_secs`).
    Periodic,
    /// Event-driven only via bundled Forgejo local webhooks (and manual/control wakes).
    Event,
    /// Instant Forgejo webhook wakes plus a long fallback timer (`wake_interval_secs`, default 300s).
    Hybrid,
}

impl WakeMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Periodic => "periodic",
            Self::Event => "event",
            Self::Hybrid => "hybrid",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "periodic" => Some(Self::Periodic),
            "event" => Some(Self::Event),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// Mechanical context reconstruction mode before each stateless wakeup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextMode {
    /// Legacy behavior: pass `instruction.md` untouched with no injected snapshot.
    #[default]
    None,
    /// Minimal role-scoped snapshot: wake reason, local git status, assigned issues, own open PRs.
    #[serde(rename = "self")]
    SelfOnly,
}

impl ContextMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SelfOnly => "self",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "self" => Some(Self::SelfOnly),
            _ => None,
        }
    }
}

/// Per-role mechanical context reconstruction configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleContextConfig {
    /// `none` (legacy default) or `self` (fresh install default).
    #[serde(default)]
    pub mode: ContextMode,
    /// Optional team-wide views this role opts into (`open_pulls`, `unassigned_issues`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<String>,
    /// Optional repository filter (`team/repo` or repo directory name). Empty = all cloned repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Optional issue label filter. Empty = all labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Optional command run inside the role's same Landlock sandbox to gather custom context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_cmd: Option<String>,
    /// Timeout for `hydrate_cmd` in seconds (default 15, max 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_timeout_secs: Option<u64>,
    /// Max captured bytes for `hydrate_cmd` output (default 16384, max 65536).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrate_max_bytes: Option<usize>,
}

/// Per-role tiered internal GitHub (Forgejo) permission switches.
///
/// Default is all false: least-privilege contributor rights (read assigned issues,
/// read issue/PR, comment, list/open/update own PRs, push branches). Repo deletion
/// and instance admin are never granted to any role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolePermissions {
    /// Opt-in: list all repo issues, create issues, assign/label/close/reopen issues.
    #[serde(default, skip_serializing_if = "is_false")]
    pub triage: bool,
    /// Opt-in: list all repo PRs, read PR diffs, submit formal reviews (self-review hard-blocked).
    #[serde(default, skip_serializing_if = "is_false")]
    pub review: bool,
    /// Opt-in: merge approved non-own PRs, create new team repositories (never delete).
    #[serde(default, skip_serializing_if = "is_false")]
    pub maintain: bool,
    /// Optional per-run Forgejo write cap override (default 8, clamped 1..=64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_bound: Option<u32>,
}

impl RolePermissions {
    pub fn effective_write_bound(&self) -> u32 {
        self.write_bound
            .unwrap_or(crate::forgejo_tools::WRITE_BOUND)
            .clamp(1, 64)
    }

    pub fn is_default(&self) -> bool {
        !self.triage && !self.review && !self.maintain && self.write_bound.is_none()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Validate a role-scoped external environment variable name, rejecting internal,
/// loader, shell-hook and harness-home variables that could weaken confinement.
pub fn validate_external_env_key(key: &str) -> Result<(), String> {
    let trimmed = key.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(format!("environment variable name `{key}` is invalid"));
    }
    let mut chars = trimmed.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "environment variable name `{key}` must match [A-Za-z_][A-Za-z0-9_]*"
        ));
    }
    let upper = trimmed.to_ascii_uppercase();
    let forbidden_exact = [
        "PATH",
        "HOME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SHELL",
        "USER",
        "LOGNAME",
        "TERM",
        "IFS",
        "ENV",
        "BASH_ENV",
        "CDPATH",
        "GLOBIGNORE",
        "SHELLOPTS",
        "BASH_XTRACEFD",
        "PS4",
        "PROMPT_COMMAND",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "KRB5CCNAME",
        "DBUS_SESSION_BUS_ADDRESS",
        "NODE_OPTIONS",
        "NODE_PATH",
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "JAVA_TOOL_OPTIONS",
        "_JAVA_OPTIONS",
        "PYTHONHOME",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "PERL5OPT",
        "RUBYOPT",
        "RUSTC_WRAPPER",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "LOCPATH",
        "GCONV_PATH",
        "HOSTALIASES",
        "RESOLV_HOST_CONF",
        "RES_OPTIONS",
        "OPENCODE_PASSWORD",
    ];
    let forbidden_prefix = [
        "LD_",
        "DYLD_",
        "GEWU_",
        "GIT_",
        "XDG_",
        "DSH_",
        "CODEX_",
        "PI_",
        "OPENCODE_",
        "BASH_FUNC_",
    ];
    if forbidden_exact.contains(&upper.as_str())
        || forbidden_prefix.iter().any(|p| upper.starts_with(p))
    {
        return Err(format!(
            "environment variable `{key}` is reserved by the runtime boundary and cannot be set per role"
        ));
    }
    Ok(())
}

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
    /// and is loaded as `Some("dsh")`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_harness: Option<String>,
    /// Global default wake mode (`periodic`, `event`, or `hybrid`). Absent in legacy
    /// files resolves to `periodic`; fresh installations default to `hybrid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mode: Option<String>,
    /// Global default mechanical context mode (`none` or `self`). Absent in legacy
    /// files resolves to `none`; fresh installations default to `self`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context: Option<String>,
    /// Idle timeout in seconds before a per-role resident harness server is parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_idle_secs: Option<u64>,
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
    /// explicit compatibility opt-in that runs Codex without its own sandbox inside the
    /// product's enforced Landlock.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codex_sandbox: String,
    /// Optional per-role wake mode override (`periodic`, `event`, or `hybrid`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mode: Option<String>,
    /// Optional wake polling / fallback interval in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_interval_secs: Option<u64>,
    /// Optional per-role mechanical context reconstruction configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<RoleContextConfig>,
    /// Optional per-role tiered Forgejo permissions (default: least privilege).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<RolePermissions>,
    /// Optional non-secret external environment variables for this role's workspace
    /// tools, `hydrate_cmd` and custom `mcp.json`. Secrets live in 0600 `credentials.toml`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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

    /// The saved program value that this build does not know, if any. An unknown
    /// value is never silently treated as built-in; it stays saved for repair.
    pub fn unknown_harness(&self) -> Option<String> {
        let saved = self.harness.trim();
        (!saved.is_empty() && Program::parse(saved).is_none()).then(|| saved.to_owned())
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

    /// Effective wake mode for this role, inheriting global default when omitted.
    pub fn effective_wake_mode(&self, global: WakeMode) -> WakeMode {
        self.wake_mode
            .as_deref()
            .and_then(WakeMode::parse)
            .unwrap_or(global)
    }

    /// Effective wake polling / fallback interval in seconds.
    pub fn effective_wake_interval_secs(&self, mode: WakeMode) -> u64 {
        match self.wake_interval_secs {
            Some(secs) => secs.clamp(5, 86_400),
            None => match mode {
                WakeMode::Periodic => 20,
                WakeMode::Hybrid => DEFAULT_HYBRID_INTERVAL_SECS,
                WakeMode::Event => 0,
            },
        }
    }

    /// Effective mechanical context reconstruction config for this role.
    pub fn effective_context(&self, global_mode: ContextMode) -> RoleContextConfig {
        match &self.context {
            Some(cfg) => cfg.clone(),
            None => RoleContextConfig {
                mode: global_mode,
                ..RoleContextConfig::default()
            },
        }
    }

    /// Effective Forgejo permissions for this role (defaults to least privilege).
    pub fn effective_permissions(&self) -> RolePermissions {
        self.permissions.clone().unwrap_or_default()
    }

    /// Validate all external environment variable names configured on this role.
    pub fn validate_env(&self) -> Result<(), String> {
        for key in self.env.keys() {
            validate_external_env_key(key)?;
        }
        Ok(())
    }
}

impl Settings {
    /// Global default wake mode. Absent in legacy files resolves to `Periodic` so
    /// existing deployments experience zero scheduling drift.
    pub fn effective_wake_mode(&self) -> WakeMode {
        self.wake_mode
            .as_deref()
            .and_then(WakeMode::parse)
            .unwrap_or(WakeMode::Periodic)
    }

    /// Global default context mode. Absent in legacy files resolves to `None` so
    /// existing prompts stay byte-for-byte unchanged.
    pub fn effective_default_context(&self) -> ContextMode {
        self.default_context
            .as_deref()
            .and_then(ContextMode::parse)
            .unwrap_or(ContextMode::None)
    }

    /// Idle park timeout (seconds) for per-role resident harness servers.
    pub fn effective_runtime_idle_secs(&self) -> u64 {
        self.runtime_idle_secs
            .unwrap_or(DEFAULT_RUNTIME_IDLE_SECS)
            .clamp(30, 3600)
    }

    /// Read the file, or the defaults when it is not there.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut settings: Self = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| format!("{} is not valid: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    default_harness: Some(Program::Dsh.id().to_owned()),
                    wake_mode: Some(WakeMode::Hybrid.id().to_owned()),
                    default_context: Some(ContextMode::SelfOnly.id().to_owned()),
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
            wake_mode: None,
            wake_interval_secs: None,
            context: None,
            permissions: None,
            env: BTreeMap::new(),
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
        assert_eq!(settings.default_harness.as_deref(), Some("dsh"));
        assert_eq!(settings.effective_wake_mode(), WakeMode::Hybrid);
        assert_eq!(settings.effective_default_context(), ContextMode::SelfOnly);
    }

    /// The installer's `--harness none` leaves no agent program installed, while a
    /// fresh settings file still defaults a new agent to DeepSeek Harness (dsh).
    /// Both public texts must say that plainly instead of implying the built-in
    /// engine is selected.
    #[test]
    fn the_public_install_text_states_the_fresh_dsh_default() {
        let readme = include_str!("../../README.md");
        let installer = include_str!("../../install.sh");
        for (name, text) in [("README.md", readme), ("install.sh", installer)] {
            let flat = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            assert!(
                flat.contains("a new agent still starts as deepseek harness"),
                "{name} does not state what --harness none leaves for a new agent"
            );
            assert!(
                !flat.contains("installs only the built-in engine"),
                "{name} still says --harness none installs only the built-in engine"
            );
        }
        let root = crate::test_support::scratch_root("settings-public-text");
        let settings = Settings::load(&path_in(&root)).expect("a missing file is fresh");
        assert_eq!(settings.default_harness.as_deref(), Some("dsh"));
    }

    #[test]
    fn legacy_matrixorg_config_inherits_builtin_periodic_none_and_least_privilege() {
        let root = crate::test_support::scratch_root("settings-matrixorg-compat");
        let path = path_in(&root);
        let legacy = r#"
terms_acknowledged = true
terms_version = 1
next_agent = 2

[endpoint]
url = "http://127.0.0.1:8080/v1/chat/completions"
model = "test-model"
max_turns = 24

[forgejo]
admin_login = "owner"

[[agents]]
name = "agent-1"
desired = "stopped"
harness = ""
"#;
        fs::write(&path, legacy).expect("write legacy");
        let read = Settings::load(&path).expect("load legacy");
        assert_eq!(read.default_harness, None);
        assert_eq!(read.effective_wake_mode(), WakeMode::Periodic);
        assert_eq!(read.effective_default_context(), ContextMode::None);
        let agent = read.agent("agent-1").expect("agent-1");
        assert_eq!(agent.harness(), Program::Builtin);
        assert_eq!(agent.desired(), Desired::Stopped);
        assert_eq!(
            agent.effective_wake_mode(read.effective_wake_mode()),
            WakeMode::Periodic
        );
        assert_eq!(
            agent.effective_context(read.effective_default_context()),
            RoleContextConfig::default()
        );
        assert!(agent.effective_permissions().is_default());
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
