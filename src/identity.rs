//! One Forgejo account per agent, derived from the agent's own name.
//!
//! An agent works on the Forgejo as *itself*, not as the person who runs the product.
//! The bundled instance's administrator key makes one account per agent, named
//! `gewu-<agent>`, and stores the token it makes for that account in the credentials
//! file. The account is marked when it is made, so a machine identity is never
//! mistaken for a person and a person who happens to have that name is refused rather
//! than taken over.
//!
//! The identity's token never leaves the credentials file on its way to git: git
//! asks this same program for it through `GIT_ASKPASS` (see [`crate::workspace`]), so
//! it is never a command-line argument, a URL or a log line.

use std::io::Read;

use crate::credentials::{self, CredentialsError};
use crate::forgejo::{
    DEFAULT_TIMEOUT, ForgejoClient, ForgejoError, IDENTITY_DESCRIPTION, IDENTITY_EMAIL_DOMAIN,
    LOGIN_PREFIX,
};
use crate::naming::agent_name_problem;

/// The rights version a managed token must carry before it is reused for the tools.
///
/// It is the product's own marker, written when `i` has made (or confirmed) a token
/// with the issue rights. A managed identity settled before this marker existed is
/// told that `i` renews its token; it is never silently taken to have rights it may
/// not have.
pub const TOOL_RIGHTS_VERSION: &str = "issue-tools-v1";

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
            "{} as `{}`",
            crate::words::identity_pair(&self.login, &self.email),
            self.agent,
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
    #[error("the bundled Forgejo administrator key is not ready yet; the product tries again")]
    NoAdminToken,
    /// The agent's token lacks rights the Forgejo tools need.
    #[error(
        "the agent token for `{agent}` is missing the Forgejo rights {rights}; the product makes a new token when it sets the agent up again"
    )]
    MissingRights {
        /// The agent whose token lacks them.
        agent: String,
        /// The missing rights, comma-separated, as Forgejo named them.
        rights: String,
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
            // The class always names the thing that is absent.
            IdentityError::MissingRights { .. } => "missing_scope",
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

    // Reuse a token that already works **and** was recorded with the rights the
    // tools need, so a second resolve does not churn tokens. A token made before the
    // issue tools existed has no marker (or an old one): `i` renews it below instead
    // of reusing a token whose rights are not known.
    let rights_recorded = credentials::recorded_agent_identity_rights(credentials_path, agent)?
        .as_deref()
        == Some(TOOL_RIGHTS_VERSION);
    if existing.is_some() && rights_recorded {
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
                            "managed",
                            None,
                        )?;
                        let created_at =
                            credentials::recorded_identity_created(credentials_path, agent)?;
                        return Ok(AgentIdentity {
                            agent: agent.to_owned(),
                            login,
                            email,
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
    // An account removed by this product is deactivated, not deleted. Adding the
    // agent again brings the same account and its history back.
    if existing
        .as_ref()
        .is_some_and(|user| !user.active || user.prohibit_login)
    {
        client.set_user_active(&login, true)?;
    }
    // A renewal makes a second token, and Forgejo refuses a duplicate token name:
    // the creation keeps the account's own name, while a renewal gets a name that
    // says it was made for the issue rights and when.
    let token_name = if created {
        format!("{LOGIN_PREFIX}{agent}")
    } else {
        format!(
            "{LOGIN_PREFIX}{agent}-{TOOL_RIGHTS_VERSION}-{}",
            crate::time::now_unix_ms()
        )
    };
    // The token this renewal supersedes. A managed identity made before token names
    // were recorded can only have the deterministic first name. Renaming is not
    // revoking: the old name is deleted on the server below (B1).
    let superseded = if created {
        None
    } else {
        Some(
            credentials::recorded_agent_token_name(credentials_path, agent)?
                .unwrap_or_else(|| format!("{LOGIN_PREFIX}{agent}")),
        )
    };
    let token = client.create_token_for(&login, &token_name)?;
    // The password was only ever needed to make the account; it is dropped here, and
    // only the token is written down.
    password.clear();
    // Prove the new token can read issues before the marker says it has the rights:
    // a Forgejo that silently drops a scope must not be recorded as if it kept it.
    let probe = ForgejoClient::new(url, token.clone(), DEFAULT_TIMEOUT)?;
    probe
        .assigned_issues()
        .map_err(|error| scope_error(agent, error))?;
    // Revoke the credential this renewal replaces, on the server, before the new one
    // is recorded as the agent's. A person who renews to revoke must not be left with
    // an old key that still answers. A 404 means it was already gone, which is the
    // same thing for the caller; any other refusal is reported, not swallowed.
    if let Some(name) = &superseded {
        if name != &token_name {
            client.delete_token_for(&login, name)?;
        }
    }
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
        "managed",
        created_at,
    )?;
    credentials::record_agent_identity_rights(credentials_path, agent, TOOL_RIGHTS_VERSION)?;
    credentials::record_agent_token_name(credentials_path, agent, &token_name)?;
    Ok(AgentIdentity {
        agent: agent.to_owned(),
        login,
        email,
        created,
        created_at,
    })
}

/// Turn a scope refusal into the one sentence naming the missing right.
fn scope_error(agent: &str, error: ForgejoError) -> IdentityError {
    match error {
        ForgejoError::MissingScope { rights, .. } if !rights.is_empty() => {
            IdentityError::MissingRights {
                agent: agent.to_owned(),
                rights: rights.join(", "),
            }
        }
        other => IdentityError::Forgejo(other),
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
}
