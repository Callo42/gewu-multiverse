//! One identity per agent, derived from configuration.
//!
//! An agent works on the Forgejo as *itself*, not as the person who runs the tool.
//! Which account that is comes from the configuration and the credentials file, never
//! from an edit to this source:
//!
//! - With an **administrator token** (`forgejo_admin_token`), the tool creates and
//!   manages one Forgejo account per agent, named from the agent's own name, and
//!   stores the token it makes for that account in the credentials file.
//! - **Without** one, the user supplies each agent's token under
//!   `[forgejo_agent_tokens]`, and the tool records that this is the path in use.
//!
//! Either way the same thing comes out: an [`AgentIdentity`] naming the account a
//! commit will be authored by. The account's login is
//! `gewu-<agent>` — [`LOGIN_PREFIX`] plus the agent's validated name — so a machine
//! identity is never mistaken for a person, and a person who happens to have that
//! name is refused rather than taken over.
//!
//! The identity's token never leaves the credentials file on its way to git: git
//! asks this same program for it through `GIT_ASKPASS` (see [`crate::workspace`]), so
//! it is never a command-line argument, a URL or a log line.

use std::io::Read;

use crate::config::Role;
use crate::credentials::{self, CredentialsError, Secret};
use crate::forgejo::{
    DEFAULT_TIMEOUT, ForgejoClient, ForgejoError, IDENTITY_DESCRIPTION, IDENTITY_EMAIL_DOMAIN,
    LOGIN_PREFIX,
};
use crate::naming::agent_name_problem;

/// Which path is making an agent's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMode {
    /// An administrator token is configured: the tool made the account and its token.
    Managed,
    /// The user supplied the token; the tool records which account it belongs to.
    Supplied,
}

impl IdentityMode {
    /// The word the credentials file and the screens use.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityMode::Managed => "managed",
            IdentityMode::Supplied => "supplied",
        }
    }

    /// Read the word back. An unknown word is `None`, never a guess.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "managed" => Some(IdentityMode::Managed),
            "supplied" => Some(IdentityMode::Supplied),
            _ => None,
        }
    }

    /// A sentence for a screen.
    pub fn describe(self) -> &'static str {
        match self {
            IdentityMode::Managed => {
                "an administrator token is configured, so the tool creates and manages one account per agent"
            }
            IdentityMode::Supplied => {
                "no administrator token is configured, so each agent's token is supplied by you"
            }
        }
    }
}

/// One agent's Forgejo identity: the account its commits are authored by.
///
/// There is deliberately no token here. The token lives in the credentials file, and
/// git asks for it there; nothing that carries an identity carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    /// The agent the identity belongs to.
    pub agent: String,
    /// The Forgejo account's login.
    pub login: String,
    /// The email recorded for that account, used as the commit email.
    pub email: String,
    /// Which path produced it.
    pub mode: IdentityMode,
    /// Whether this call created the account (as opposed to finding it).
    pub created: bool,
    /// When the tool created the account, in unix seconds, when it knows.
    ///
    /// Recorded so a report read minutes or days later says when the account was
    /// made instead of forever claiming `created now` (H-3).
    pub created_at: Option<u64>,
}

impl AgentIdentity {
    /// One line for a screen.
    pub fn line(&self) -> String {
        format!(
            "{} <{}> as `{}` ({})",
            self.login,
            self.email,
            self.agent,
            self.mode.as_str(),
        )
    }
}

/// Why an agent identity could not be settled.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The agent's name cannot name an account.
    #[error("the agent name `{name}` {problem}")]
    UnsafeName {
        /// The name that was asked for.
        name: String,
        /// What is wrong with it.
        problem: String,
    },
    /// The configuration says an administrator should manage identities, but no
    /// administrator token is in the credentials file.
    //
    // A-2's class: a refusal a user acts on names the product's own keys for the fix
    // (`A` for the administrator token, `v` for the agent's), never a file and a TOML
    // section. The credentials file is still named on the Credentials screen.
    #[error(
        "no administrator token is configured, so no agent identity can be created; press A on the Credentials screen to add one, or v on the Forgejo screen to supply the agent's own token"
    )]
    NoAdminToken,
    /// The role check needs `forgejo_token`, and none is recorded.
    ///
    // T-4: the sibling of `NoAgentToken`. A refusal a user acts on names the
    // product's own keys for the fix (`f` on the Credentials screen writes
    // `forgejo_token`), never a file and a TOML section; the credentials file is
    // still named on the Credentials screen itself.
    #[error(
        "role token absent — press 5 and then f on the Credentials screen to add one"
    )]
    NoRoleToken,
    /// No token was supplied for this agent, and there is no administrator token.
    ///
    // N-3: the surviving sibling of the round-9 class. The refusal a user acts on
    // opens with the thing that is missing and names the product's own key for the
    // fix (`v` on the Forgejo screen), never a file and a TOML section; the
    // credentials file is still named on the Credentials screen itself.
    #[error("the agent token for `{agent}` is absent — press v on the Forgejo screen to supply it")]
    NoAgentToken {
        /// The agent whose token is absent.
        agent: String,
    },
    /// The account exists but was not made by this tool, so it is not taken over.
    #[error(
        "the Forgejo account `{login}` already exists and is not marked as one this tool made; refusing to take it over"
    )]
    NotOurs {
        /// The account that was found.
        login: String,
    },
    /// The credentials file could not be read or written.
    #[error(transparent)]
    Credentials(#[from] CredentialsError),
    /// The Forgejo refused a request.
    #[error(transparent)]
    Forgejo(#[from] ForgejoError),
    /// A password could not be generated for a new identity.
    #[error("a password for the new identity could not be generated: {0}")]
    Random(std::io::Error),
}

impl IdentityError {
    /// The class of failure, as one word a screen can hold.
    pub fn class(&self) -> &'static str {
        match self {
            IdentityError::UnsafeName { .. } => "unsafe_name",
            IdentityError::NoAdminToken => "no_admin_token",
            IdentityError::NoRoleToken => "credentials",
            IdentityError::NoAgentToken { .. } => "no_agent_token",
            IdentityError::NotOurs { .. } => "not_ours",
            IdentityError::Credentials(_) => "credentials",
            IdentityError::Forgejo(error) => error.class(),
            IdentityError::Random(_) => "random",
        }
    }
}

/// The Forgejo account login an agent's identity uses, from the agent's own name.
pub fn login_for(agent: &str) -> Result<String, IdentityError> {
    if let Some(problem) = agent_name_problem(agent) {
        return Err(IdentityError::UnsafeName {
            name: agent.to_owned(),
            problem,
        });
    }
    Ok(format!("{LOGIN_PREFIX}{agent}"))
}

/// The email an identity created by the tool uses.
pub fn email_for(login: &str) -> String {
    format!("{login}@{IDENTITY_EMAIL_DOMAIN}")
}

/// Settle the identity of one agent, creating or finding it as the path requires.
///
/// An administrator token means the tool manages the account; without one, the token
/// the user supplied is checked and its account recorded. Either way the credentials
/// file ends up naming the login and email a commit will use.
pub fn resolve(
    url: &str,
    agent: &str,
    credentials_path: &std::path::Path,
) -> Result<AgentIdentity, IdentityError> {
    if credentials::forgejo_admin_token(credentials_path)?.is_some() {
        manage(url, agent, credentials_path)
    } else {
        supplied(url, agent, credentials_path)
    }
}

/// Which identity path the credentials file is on, when it can be told.
pub fn recorded_mode(
    credentials_path: &std::path::Path,
) -> Result<Option<IdentityMode>, IdentityError> {
    if let Some(word) = credentials::recorded_identity_mode(credentials_path)? {
        if let Some(mode) = IdentityMode::parse(&word) {
            return Ok(Some(mode));
        }
    }
    if credentials::forgejo_admin_token(credentials_path)?.is_some() {
        return Ok(Some(IdentityMode::Managed));
    }
    Ok(None)
}

/// The recorded login for an agent, when one was recorded.
pub fn recorded_login(
    credentials_path: &std::path::Path,
    agent: &str,
) -> Result<Option<String>, IdentityError> {
    Ok(credentials::recorded_login(credentials_path, agent)?)
}

/// The recorded email for an agent, when one was recorded.
pub fn recorded_email(
    credentials_path: &std::path::Path,
    agent: &str,
) -> Result<Option<String>, IdentityError> {
    Ok(credentials::recorded_email(credentials_path, agent)?)
}

/// Create or refresh one agent's account and token, as an administrator.
///
/// The account is only ever made once. An existing account is reused when it carries
/// the tool's mark; one that does not is refused, because a person's account is not
/// the tool's to take over. Refreshing means setting a freshly generated password
/// (kept for the length of one request, never written down) and making a new token —
/// necessary because a token for another account can only be created with that
/// account's own credentials.
pub fn manage(
    url: &str,
    agent: &str,
    credentials_path: &std::path::Path,
) -> Result<AgentIdentity, IdentityError> {
    let login = login_for(agent)?;
    let admin =
        credentials::forgejo_admin_token(credentials_path)?.ok_or(IdentityError::NoAdminToken)?;
    let client = ForgejoClient::new(url, admin, DEFAULT_TIMEOUT)?;

    let existing = client.user(&login)?;
    if let Some(user) = &existing {
        if user.description.as_deref() != Some(IDENTITY_DESCRIPTION) {
            return Err(IdentityError::NotOurs { login });
        }
    }

    // Reuse a token that already works, so a second resolve does not churn tokens.
    if existing.is_some() {
        if let Some(token) = credentials::forgejo_agent_token_optional(credentials_path, agent)? {
            if let Ok(existing_client) = ForgejoClient::new(url, token, DEFAULT_TIMEOUT) {
                if let Ok(viewer) = existing_client.viewer() {
                    if viewer.login == login {
                        let email = viewer
                            .email
                            .clone()
                            .filter(|email| !email.is_empty())
                            .unwrap_or_else(|| email_for(&login));
                        credentials::record_agent_identity(
                            credentials_path,
                            agent,
                            None,
                            &login,
                            &email,
                            IdentityMode::Managed.as_str(),
                            None,
                        )?;
                        let created_at =
                            credentials::recorded_identity_created(credentials_path, agent)?;
                        return Ok(AgentIdentity {
                            agent: agent.to_owned(),
                            login,
                            email,
                            mode: IdentityMode::Managed,
                            created: false,
                            created_at,
                        });
                    }
                }
            }
        }
    }

    let created = existing.is_none();
    let email = email_for(&login);
    let mut password = generate_password()?;
    if created {
        client.create_user(&login, &email, &password)?;
    }
    // The mark is written on every manage, so a fresh account can be recognised
    // afterwards and an old one is not mistaken for a person.
    client.mark_identity(&login, &format!("{LOGIN_PREFIX}{agent}"))?;
    let token = client.create_token_for(&login, &format!("{LOGIN_PREFIX}{agent}"))?;
    // The password was only ever needed to make the account; it is dropped here, and
    // only the token is written down.
    password.clear();
    // The account's creation time is written down the moment it is made, so the
    // report can age instead of repeating `now` (H-3).
    let created_at = if created {
        Some(crate::time::now_unix_ms() / 1000)
    } else {
        credentials::recorded_identity_created(credentials_path, agent)?
    };
    credentials::record_agent_identity(
        credentials_path,
        agent,
        Some(&token),
        &login,
        &email,
        IdentityMode::Managed.as_str(),
        created_at,
    )?;
    Ok(AgentIdentity {
        agent: agent.to_owned(),
        login,
        email,
        mode: IdentityMode::Managed,
        created,
        created_at,
    })
}

/// Check the token the user supplied for one agent and record whose it is.
pub fn supplied(
    url: &str,
    agent: &str,
    credentials_path: &std::path::Path,
) -> Result<AgentIdentity, IdentityError> {
    let login_hint = login_for(agent)?;
    let _ = login_hint;
    let token = credentials::forgejo_agent_token(credentials_path, agent).map_err(|error| {
        if matches!(error, CredentialsError::MissingToken { .. }) {
            IdentityError::NoAgentToken {
                agent: agent.to_owned(),
            }
        } else {
            IdentityError::Credentials(error)
        }
    })?;
    let client = ForgejoClient::new(url, token, DEFAULT_TIMEOUT)?;
    let viewer = client.viewer()?;
    let login = viewer.login.clone();
    let email = viewer
        .email
        .clone()
        .filter(|email| !email.is_empty())
        .unwrap_or_else(|| email_for(&login));
    credentials::record_agent_identity(
        credentials_path,
        agent,
        None,
        &login,
        &email,
        IdentityMode::Supplied.as_str(),
        None,
    )?;
    Ok(AgentIdentity {
        agent: agent.to_owned(),
        login,
        email,
        mode: IdentityMode::Supplied,
        created: false,
        created_at: None,
    })
}

/// The role check token, and whether it should be an administrator's.
///
/// The token in `forgejo_token` is the one the role check uses. When the
/// configuration says `role = "admin"` and that token is absent, the administrator
/// token is used instead, so one token can serve both purposes.
pub fn role_token(credentials_path: &std::path::Path, role: Role) -> Result<Secret, IdentityError> {
    match credentials::forgejo_token(credentials_path) {
        Ok(token) => Ok(token),
        Err(CredentialsError::MissingToken { .. }) if role == Role::Admin => {
            credentials::forgejo_admin_token(credentials_path)?.ok_or(IdentityError::NoAdminToken)
        }
        Err(CredentialsError::MissingToken { .. }) => Err(IdentityError::NoRoleToken),
        Err(error) => Err(IdentityError::Credentials(error)),
    }
}

/// A generated password for a new identity: enough entropy, never written down.
fn generate_password() -> Result<String, IdentityError> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(IdentityError::Random)?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("Gw-{hex}!9"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_login_is_derived_from_the_agents_own_name() {
        assert_eq!(login_for("scribe").expect("accepted"), "gewu-scribe");
        assert_eq!(login_for("checker-2").expect("accepted"), "gewu-checker-2");
        assert!(email_for("gewu-scribe").ends_with("@agents.invalid"));
    }

    #[test]
    fn a_name_that_cannot_name_an_account_is_refused() {
        let error = login_for("Bad Name").expect_err("refused");
        assert_eq!(error.class(), "unsafe_name");
        assert!(
            error.to_string().contains("cannot name a directory")
                || error.to_string().contains("not allowed")
        );
    }

    #[test]
    fn the_mode_words_round_trip() {
        for mode in [IdentityMode::Managed, IdentityMode::Supplied] {
            assert_eq!(IdentityMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(IdentityMode::parse("something-else"), None);
    }

    #[test]
    fn the_login_prefix_keeps_a_machine_identity_apart_from_a_person() {
        let login = login_for("scribe").expect("accepted");
        assert!(login.starts_with(LOGIN_PREFIX));
        // Longest accepted agent name still fits a Forgejo login (40 characters).
        let longest = "a".repeat(crate::naming::MAX_AGENT_NAME_LEN);
        let login = login_for(&longest).expect("accepted");
        assert!(login.len() <= 40, "{login}");
    }

    #[test]
    fn an_identity_line_names_the_account_and_the_path() {
        let identity = AgentIdentity {
            agent: "scribe".to_owned(),
            login: "gewu-scribe".to_owned(),
            email: "gewu-scribe@agents.invalid".to_owned(),
            mode: IdentityMode::Managed,
            created: true,
            created_at: Some(1_700_000_000),
        };
        let line = identity.line();
        assert!(line.contains("gewu-scribe"), "{line}");
        assert!(line.contains("managed"), "{line}");
    }

    #[test]
    fn every_token_refusal_names_the_key_a_person_can_press() {
        // N-3: the refusal class has had survivors in three rounds because the sweep
        // followed the finding's own file rather than the code. This guard reads the
        // refusal strings themselves: a sentence about a missing token must name the
        // product's own key and must not send a person to a file or a TOML section.
        let refusals = [
            IdentityError::NoRoleToken.to_string(),
            IdentityError::NoAgentToken {
                agent: "scribe".to_owned(),
            }
            .to_string(),
            IdentityError::NoAdminToken.to_string(),
        ];
        for refusal in &refusals {
            assert!(
                refusal.contains("absent") || refusal.contains("no administrator token"),
                "a token refusal does not say what is missing: {refusal}"
            );
            assert!(
                !refusal.contains(".toml") && !refusal.contains("credentials file"),
                "a token refusal sends a person to a file: {refusal}"
            );
            assert!(
                refusal.contains("press"),
                "a token refusal names no key: {refusal}"
            );
        }
    }
}
