//! The one configuration file: the Forgejo, the role to act as, and the agents.
//!
//! The file is TOML and is read strictly:
//!
//! - a syntax mistake is reported with the line the parser stopped at;
//! - an unknown field is refused, so a misspelled key never silently does nothing;
//! - a secret-like key is refused outright, because secrets belong in the
//!   credentials file and nowhere else;
//! - every value is then checked, and the first problem is reported as a sentence
//!   naming the field and what would be acceptable.
//!
//! Nothing in this module panics: a bad file is a value of [`ConfigError`].

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// The agent-name rule lives in [`crate::naming`], because directories derive from it.
pub use crate::naming::{MAX_AGENT_NAME_LEN, agent_name_problem, is_safe_agent_name};

/// Key names that may never appear in the configuration file. A file that carries
/// one is refused, and the message points at the credentials file instead.
const SECRET_KEY_NAMES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passphrase",
    "api_key",
    "apikey",
    "private_key",
    "client_secret",
];

/// The largest number of model requests one agent may keep in flight at once.
pub const MAX_CONCURRENCY: u16 = 16;

/// The largest number of turns one run may take before it is stopped.
pub const MAX_TURNS: u32 = 256;

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The one Forgejo this Universe talks to.
    pub forgejo: Forgejo,
    /// The run limits every agent is admitted with.
    #[serde(default)]
    pub limits: Limits,
    /// The agents, in the sequence the file lists them. An empty list is allowed.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

/// The bounds every run of this installation is held to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// How long one run may last before it is stopped, in seconds.
    #[serde(default = "default_run_timeout_secs")]
    pub run_timeout_secs: u64,
    /// How many agents may run at once. A start beyond this waits its turn.
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: u16,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            run_timeout_secs: default_run_timeout_secs(),
            max_concurrent_agents: default_max_concurrent_agents(),
        }
    }
}

/// The default per-run timeout: fifteen minutes.
fn default_run_timeout_secs() -> u64 {
    900
}

/// The default limit on agents running at once.
fn default_max_concurrent_agents() -> u16 {
    10
}

/// The longest a run timeout may be: one day.
pub const MAX_RUN_TIMEOUT_SECS: u64 = 86_400;

/// The largest the limit on agents running at once may be.
pub const MAX_CONCURRENT_AGENTS: u16 = 64;

/// The Forgejo connection: where it is, and which role to act as.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forgejo {
    /// The base URL of the Forgejo, for example `https://forgejo.example.org`.
    pub url: String,
    /// The role to act as when the tool looks at repositories.
    pub role: Role,
}

/// The role to act as. An admin credential can see administrative views; a member
/// credential cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// An administrator of the Forgejo.
    Admin,
    /// An ordinary account on the Forgejo.
    Member,
}

impl Role {
    /// The word the configuration file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
}

/// One agent, as the configuration file describes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The agent's name. It is bounded and validated, because paths derive from it.
    pub name: String,
    /// The model this agent must use.
    pub model: String,
    /// The endpoint that serves that model.
    pub endpoint: String,
    /// How many model turns one run may take before the declared bound stops it.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// The instruction document, relative to the configuration directory.
    pub instruction: PathBuf,
    /// How many model requests this agent may keep in flight at once.
    pub concurrency: u16,
}

/// The default bound on model turns in one run.
fn default_max_turns() -> u32 {
    24
}

/// Everything that can go wrong while reading the configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file is not there. In practice the caller writes a sample first.
    #[error("{path} is not there yet; write it, or delete the directory to get a fresh sample")]
    Missing {
        /// The file that was expected.
        path: PathBuf,
    },
    /// The file was there but could not be read.
    #[error("{path} cannot be read: {source}")]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file is not valid TOML.
    #[error("{path} is not valid TOML: {message}")]
    Syntax {
        /// The file that failed.
        path: PathBuf,
        /// What the parser said, with the line it stopped at.
        message: String,
    },
    /// The file carries a secret-like key, which belongs in the credentials file.
    #[error(
        "{path} holds the secret-like key `{key}` under `{section}`; secrets belong in the credentials file, never in the configuration file"
    )]
    SecretInConfig {
        /// The file that failed.
        path: PathBuf,
        /// The table the key sat in.
        section: String,
        /// The key itself.
        key: String,
    },
    /// A value is outside what the program accepts.
    #[error("{path}: {field} {problem}")]
    Invalid {
        /// The file that failed.
        path: PathBuf,
        /// The field, written as it appears in the file.
        field: String,
        /// What is wrong and what would be acceptable.
        problem: String,
    },
    /// The file was written on first start but could not be.
    #[error("{path} could not be written: {source}")]
    Write {
        /// The file that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        #[source]
        source: io::Error,
    },
    /// The file did not exist, so a commented sample was written in its place.
    #[error(
        "{path} did not exist, so a commented sample was written there; edit it, then press r to re-read it"
    )]
    SampleWritten {
        /// The sample that was written.
        path: PathBuf,
    },
}

impl Config {
    /// Read and check the configuration file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(ConfigError::Missing {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Config::from_text(path, &text)
    }

    /// Read a configuration from text that is already in memory.
    ///
    /// The file is parsed once into a generic value so that a secret-like key can be
    /// named precisely, and then into the typed form, which refuses unknown fields.
    pub fn from_text(path: &Path, text: &str) -> Result<Config, ConfigError> {
        let value: toml::Value = toml::from_str(text).map_err(|error| ConfigError::Syntax {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        find_secret_key(&value, "the top level", path)?;
        let config: Config = toml::from_str(text).map_err(|error| ConfigError::Syntax {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        config.check(path)?;
        Ok(config)
    }

    /// Read the configuration file, writing a commented sample when it is absent.
    ///
    /// A fresh installation is the case this exists for: the user gets a file that
    /// shows every field and explains itself, and the message says so.
    pub fn load_or_write_sample(path: &Path) -> Result<Config, ConfigError> {
        if path.exists() {
            return Config::load(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        fs::write(path, SAMPLE).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        Err(ConfigError::SampleWritten {
            path: path.to_path_buf(),
        })
    }

    /// Check every value. The first problem found is returned.
    pub fn check(&self, path: &Path) -> Result<(), ConfigError> {
        let url = self.forgejo.url.trim();
        if url.is_empty() {
            return Err(invalid(path, "forgejo.url", "is empty; it needs a URL"));
        }
        if let Some(problem) = url_problem(url) {
            return Err(invalid(path, "forgejo.url", format!("= {url:?} {problem}")));
        }

        if self.limits.run_timeout_secs == 0 {
            return Err(invalid(
                path,
                "limits.run_timeout_secs",
                "= 0 is not allowed; a run needs a timeout of at least 1 second",
            ));
        }
        if self.limits.run_timeout_secs > MAX_RUN_TIMEOUT_SECS {
            return Err(invalid(
                path,
                "limits.run_timeout_secs",
                format!(
                    "= {} is too large; it must be at most {MAX_RUN_TIMEOUT_SECS}",
                    self.limits.run_timeout_secs
                ),
            ));
        }
        if self.limits.max_concurrent_agents == 0 {
            return Err(invalid(
                path,
                "limits.max_concurrent_agents",
                "= 0 is not allowed; at least one agent must be able to run",
            ));
        }
        if self.limits.max_concurrent_agents > MAX_CONCURRENT_AGENTS {
            return Err(invalid(
                path,
                "limits.max_concurrent_agents",
                format!(
                    "= {} is too large; it must be at most {MAX_CONCURRENT_AGENTS}",
                    self.limits.max_concurrent_agents
                ),
            ));
        }

        let mut names: Vec<&str> = Vec::with_capacity(self.agents.len());
        for (index, agent) in self.agents.iter().enumerate() {
            let field = |name: &str| format!("agents[{index}].{name}");

            if let Some(problem) = agent_name_problem(&agent.name) {
                return Err(invalid(
                    path,
                    field("name"),
                    format!("= {:?} {problem}", agent.name),
                ));
            }
            if names.contains(&agent.name.as_str()) {
                return Err(invalid(
                    path,
                    field("name"),
                    format!("= {:?} is used twice; agent names must differ", agent.name),
                ));
            }
            names.push(&agent.name);

            if agent.model.trim().is_empty() {
                return Err(invalid(
                    path,
                    field("model"),
                    "is empty; it needs a model name",
                ));
            }
            let endpoint = agent.endpoint.trim();
            if let Some(problem) = url_problem(endpoint) {
                return Err(invalid(
                    path,
                    field("endpoint"),
                    format!("= {endpoint:?} {problem}"),
                ));
            }
            if let Some(problem) = instruction_problem(&agent.instruction) {
                return Err(invalid(
                    path,
                    field("instruction"),
                    format!("= {:?} {problem}", agent.instruction.display()),
                ));
            }
            if agent.concurrency == 0 {
                return Err(invalid(
                    path,
                    field("concurrency"),
                    "= 0 is not allowed; it must be at least 1",
                ));
            }
            if agent.concurrency > MAX_CONCURRENCY {
                return Err(invalid(
                    path,
                    field("concurrency"),
                    format!(
                        "= {} is too large; it must be at most {MAX_CONCURRENCY}",
                        agent.concurrency
                    ),
                ));
            }
            if agent.max_turns == 0 {
                return Err(invalid(
                    path,
                    field("max_turns"),
                    "= 0 is not allowed; a run needs at least one turn to call the model",
                ));
            }
            if agent.max_turns > MAX_TURNS {
                return Err(invalid(
                    path,
                    field("max_turns"),
                    format!(
                        "= {} is too large; it must be at most {MAX_TURNS}",
                        agent.max_turns
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// A short, commented configuration file for a fresh installation.
pub const SAMPLE: &str = r#"# gewu-multiverse configuration
#
# One Forgejo, one role, and the agents you define.
# Secrets never go in this file: they live in credentials.toml beside it.

[forgejo]
# The Forgejo this Universe works with.
url = "https://forgejo.example.org"
# The role to act as: "admin" (administrative views) or "member" (ordinary).
role = "member"

[limits]
# How long one run may last before it is stopped, in seconds (1 to 86400).
run_timeout_secs = 900
# How many agents may run at once (1 to 64). A start beyond this waits its turn.
max_concurrent_agents = 10

# One entry per agent. Remove the block if you want no agents.
[[agents]]
name = "example-agent"
model = "example-model"
endpoint = "https://model.example.org/v1"
# How many model turns one run may take before the declared bound stops it (1 to 256).
max_turns = 24
# Relative to this file's directory. The tool also derives an agent's own directory
# under agents/<name>/, holding its instruction.md, its skills/ and its workspace/.
instruction = "instructions/example-agent.md"
# How many model requests this agent may keep in flight at once (1 to 16).
concurrency = 2
"#;

/// What is wrong with a URL, if anything.
fn url_problem(url: &str) -> Option<&'static str> {
    let rest = match url.strip_prefix("https://") {
        Some(rest) => rest,
        None => match url.strip_prefix("http://") {
            Some(rest) => rest,
            None => return Some("is not a URL; it must start with http:// or https://"),
        },
    };
    if rest.is_empty() {
        return Some("names no host");
    }
    if url.chars().any(char::is_whitespace) {
        return Some("holds whitespace");
    }
    None
}

/// What is wrong with an instruction path, if anything.
fn instruction_problem(path: &Path) -> Option<&'static str> {
    if path.as_os_str().is_empty() {
        return Some("is empty; it needs a path");
    }
    if path.is_absolute() {
        return Some("is absolute; it must be relative to the configuration directory");
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Some("holds `..`, which would leave the configuration directory");
    }
    None
}

/// Walk the parsed file and refuse the first secret-like key.
fn find_secret_key(value: &toml::Value, section: &str, path: &Path) -> Result<(), ConfigError> {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                if SECRET_KEY_NAMES.contains(&key.to_ascii_lowercase().as_str()) {
                    return Err(ConfigError::SecretInConfig {
                        path: path.to_path_buf(),
                        section: section.to_owned(),
                        key: key.clone(),
                    });
                }
                let child_section = format!("{section}.{key}");
                find_secret_key(child, &child_section, path)?;
            }
        }
        toml::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let child_section = format!("{section}[{index}]");
                find_secret_key(child, &child_section, path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Build the "a value is outside what is accepted" error.
fn invalid(path: &Path, field: impl Into<String>, problem: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        field: field.into(),
        problem: problem.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const GOOD: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[[agents]]
name = "example-agent"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "instructions/example-agent.md"
concurrency = 2
"#;

    fn path() -> &'static Path {
        Path::new("/tmp/example/config.toml")
    }

    fn message(text: &str) -> String {
        Config::from_text(path(), text)
            .expect_err("this configuration must be refused")
            .to_string()
    }

    #[test]
    fn a_good_file_is_read() {
        let config = Config::from_text(path(), GOOD).expect("the good file is accepted");
        assert_eq!(config.forgejo.url, "https://forgejo.example.org");
        assert_eq!(config.forgejo.role, Role::Member);
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "example-agent");
        assert_eq!(config.agents[0].concurrency, 2);
    }

    #[test]
    fn the_sample_is_a_valid_configuration() {
        let config = Config::from_text(path(), SAMPLE).expect("the sample is accepted");
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn the_sample_holds_no_secret_like_key() {
        let value: toml::Value = toml::from_str(SAMPLE).expect("the sample parses");
        find_secret_key(&value, "the top level", path()).expect("no secret-like key in the sample");
    }

    #[test]
    fn the_sample_never_names_a_secret_key_even_in_a_comment() {
        // A comment that looked like a key would still be a bad example to copy.
        for name in SECRET_KEY_NAMES {
            assert!(
                !SAMPLE.contains(name),
                "the sample mentions {name}, which reads as an invitation"
            );
        }
    }

    #[test]
    fn a_secret_like_key_is_refused_with_a_clear_message() {
        let text = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"
token = "abcdef"
"#;
        let message = message(text);
        assert!(message.contains("credentials file"), "{message}");
        assert!(message.contains("token"), "{message}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let text = GOOD.replace("concurrency = 2", "concurency = 2");
        let message = message(&text);
        assert!(message.contains("unknown field"), "{message}");
    }

    #[test]
    fn a_broken_file_is_refused_with_the_parser_message() {
        let message = message("[forgejo\nurl = 3");
        assert!(message.contains("not valid TOML"), "{message}");
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_and_the_field_is_named() {
        let text = GOOD.replace("https://forgejo.example.org", "forgejo.example.org");
        let message = message(&text);
        assert!(message.contains("forgejo.url"), "{message}");
        assert!(message.contains("http://"), "{message}");
    }

    #[test]
    fn an_unknown_role_is_refused() {
        let text = GOOD.replace("role = \"member\"", "role = \"root\"");
        let message = message(&text);
        assert!(message.contains("unknown variant"), "{message}");
    }

    #[test]
    fn a_duplicate_agent_name_is_refused() {
        let text = format!("{GOOD}{}", agent_block("example-agent"));
        let message = message(&text);
        assert!(message.contains("used twice"), "{message}");
    }

    #[test]
    fn a_doubled_hyphen_in_a_name_is_refused() {
        let text = GOOD.replace("name = \"example-agent\"", "name = \"example--agent\"");
        let message = message(&text);
        assert!(message.contains("doubled hyphen"), "{message}");
    }

    #[test]
    fn an_uppercase_name_is_refused() {
        let text = GOOD.replace("name = \"example-agent\"", "name = \"Example\"");
        assert!(message(&text).contains("lowercase"));
    }

    #[test]
    fn a_name_that_is_too_long_is_refused() {
        let long = "a".repeat(MAX_AGENT_NAME_LEN + 1);
        let text = GOOD.replace("name = \"example-agent\"", &format!("name = \"{long}\""));
        assert!(message(&text).contains("longer than"));
    }

    #[test]
    fn a_concurrency_of_zero_is_refused() {
        let text = GOOD.replace("concurrency = 2", "concurrency = 0");
        let message = message(&text);
        assert!(message.contains("concurrency"), "{message}");
        assert!(message.contains("at least 1"), "{message}");
    }

    #[test]
    fn a_concurrency_above_the_bound_is_refused() {
        let text = GOOD.replace("concurrency = 2", "concurrency = 17");
        assert!(message(&text).contains("at most 16"));
    }

    #[test]
    fn the_default_turn_bound_is_used_when_the_field_is_absent() {
        let config = Config::from_text(path(), GOOD).expect("the good file is accepted");
        assert_eq!(config.agents[0].max_turns, 24);
    }

    #[test]
    fn a_turn_bound_can_be_set() {
        let text = GOOD.replace(
            "endpoint = \"https://model.example.org/v1\"",
            "endpoint = \"https://model.example.org/v1\"\nmax_turns = 3",
        );
        let config = Config::from_text(path(), &text).expect("the bound is accepted");
        assert_eq!(config.agents[0].max_turns, 3);
    }

    #[test]
    fn a_zero_or_oversized_turn_bound_is_refused() {
        for (value, expected) in [("0", "at least one turn"), ("257", "too large")] {
            let text = GOOD.replace(
                "endpoint = \"https://model.example.org/v1\"",
                &format!("endpoint = \"https://model.example.org/v1\"\nmax_turns = {value}"),
            );
            let message = message(&text);
            assert!(message.contains("max_turns"), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn an_absolute_instruction_path_is_refused() {
        let text = GOOD.replace(
            "instruction = \"instructions/example-agent.md\"",
            "instruction = \"/etc/passwd\"",
        );
        let message = message(&text);
        assert!(message.contains("instruction"), "{message}");
        assert!(message.contains("absolute"), "{message}");
    }

    #[test]
    fn an_instruction_path_that_climbs_out_is_refused() {
        let text = GOOD.replace(
            "instruction = \"instructions/example-agent.md\"",
            "instruction = \"../elsewhere.md\"",
        );
        assert!(message(&text).contains("leave the configuration directory"));
    }

    #[test]
    fn an_empty_agent_list_is_allowed() {
        let text = "[forgejo]\nurl = \"https://forgejo.example.org\"\nrole = \"admin\"\n";
        let config = Config::from_text(path(), text).expect("no agents is a valid state");
        assert!(config.agents.is_empty());
        assert_eq!(config.forgejo.role, Role::Admin);
    }

    #[test]
    fn a_missing_file_says_so_and_a_sample_is_written_instead() {
        let root = crate::test_support::scratch_root("config-sample");
        let file = root.join("config.toml");
        let error = Config::load_or_write_sample(&file).expect_err("a sample is written");
        assert!(matches!(error, ConfigError::SampleWritten { .. }));
        assert!(file.is_file());
        // The sample it wrote is itself a valid configuration.
        Config::load(&file).expect("the written sample is valid");
    }

    #[test]
    fn a_missing_file_on_its_own_is_reported_as_missing() {
        let root = crate::test_support::scratch_root("config-missing");
        let error = Config::load(&root.join("nothing.toml")).expect_err("the file is absent");
        assert!(matches!(error, ConfigError::Missing { .. }));
    }

    #[test]
    fn limits_have_the_expected_defaults_when_the_table_is_absent() {
        let config = Config::from_text(path(), GOOD).expect("the good file is accepted");
        assert_eq!(config.limits.run_timeout_secs, 900);
        assert_eq!(config.limits.max_concurrent_agents, 10);
        assert_eq!(Limits::default(), config.limits);
    }

    #[test]
    fn limits_can_be_set() {
        let text = format!("{GOOD}\n[limits]\nrun_timeout_secs = 3\nmax_concurrent_agents = 2\n");
        let config = Config::from_text(path(), &text).expect("the limits are accepted");
        assert_eq!(config.limits.run_timeout_secs, 3);
        assert_eq!(config.limits.max_concurrent_agents, 2);
    }

    #[test]
    fn the_sample_carries_the_default_limits() {
        let config = Config::from_text(path(), SAMPLE).expect("the sample is accepted");
        assert_eq!(config.limits.max_concurrent_agents, 10);
        assert_eq!(config.limits.run_timeout_secs, 900);
    }

    #[test]
    fn a_zero_or_oversized_limit_is_refused() {
        for (field, text, expected) in [
            (
                "limits.run_timeout_secs",
                format!("{GOOD}\n[limits]\nrun_timeout_secs = 0\n"),
                "at least 1 second",
            ),
            (
                "limits.run_timeout_secs",
                format!("{GOOD}\n[limits]\nrun_timeout_secs = 86401\n"),
                "too large",
            ),
            (
                "limits.max_concurrent_agents",
                format!("{GOOD}\n[limits]\nmax_concurrent_agents = 0\n"),
                "at least one agent",
            ),
            (
                "limits.max_concurrent_agents",
                format!("{GOOD}\n[limits]\nmax_concurrent_agents = 65\n"),
                "too large",
            ),
        ] {
            let message = message(&text);
            assert!(message.contains(field), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn an_unknown_field_under_limits_is_refused() {
        let text = format!("{GOOD}\n[limits]\ntimeout = 3\n");
        let message = message(&text);
        assert!(message.contains("unknown field"), "{message}");
    }

    fn agent_block(name: &str) -> String {
        format!(
            "\n[[agents]]\nname = \"{name}\"\nmodel = \"m\"\nendpoint = \"https://m.example.org\"\ninstruction = \"i.md\"\nconcurrency = 1\n"
        )
    }
}
