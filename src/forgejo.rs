//! The Forgejo client: one instance, one token, and the requests the tool makes.
//!
//! There is no relay and no anonymous call. Every request this module sends carries
//! the token from the credentials file in its `Authorization` header
//! ([`Secret::expose`] is opened for exactly that header and nowhere else). No token
//! ever reaches a URL, a request body, a log, a transcript or a command line, and
//! every message built from a Forgejo response is passed through [`Secret::redact`]
//! first, because a refusal can echo what was sent.
//!
//! The API is the small, stable part of the Forgejo (Gitea-compatible) REST API that
//! this tool needs:
//!
//! - `GET /api/v1/user` — who the token belongs to, and whether they are an admin;
//! - `GET /api/v1/repos/search` — the repositories the token can see, each with the
//!   caller's permission on it;
//! - `GET /api/v1/repos/{namespace}/{repo}` and its branches and pull requests — one
//!   repository's state;
//! - `POST /api/v1/admin/users` and `PATCH /api/v1/admin/users/{login}`, and
//!   `POST /api/v1/users/{login}/tokens` with that user's own credentials — creating
//!   and managing one identity per agent (see [`crate::identity`]);
//! - `POST`/`PATCH /api/v1/repos/{namespace}/{repo}/pulls[/{number}]` — opening and
//!   updating a pull request as the agent.
//!
//! Failures have a class ([`ForgejoError::class`]) so a screen can name the kind of
//! failure — a bad URL, a bad token, a token without the rights the configuration
//! expects, an operation outside the token's rights — without repeating a message.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Role;
use crate::credentials::{REDACTED, Secret};
use crate::words;

/// How long one Forgejo request may take.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many repositories the role check lists at most.
pub const MAX_REPOSITORIES: usize = 200;

/// The name the tool gives an agent identity, before the agent's own name.
pub const LOGIN_PREFIX: &str = "gewu-";

/// The description the tool writes on an identity it creates, so a later run can
/// tell its own machine identity from a person who happens to have that name.
pub const IDENTITY_DESCRIPTION: &str = "Machine identity created by gewu-multiverse.";

/// The domain agent identities use for their Forgejo email address. `.invalid` is
/// reserved and can never resolve, so a commit authored with it cannot be mistaken
/// for mail.
pub const IDENTITY_EMAIL_DOMAIN: &str = "agents.invalid";

/// What one request needs: where the instance is, which token, how long to wait.
#[derive(Clone)]
pub struct ForgejoConfig {
    /// The base URL, for example `https://forgejo.example.org`.
    pub url: String,
    /// The token, from the credentials file and nowhere else.
    pub token: Secret,
    /// How long one request may take.
    pub timeout: Duration,
}

impl ForgejoConfig {
    /// A configuration with the default timeout.
    pub fn new(url: impl Into<String>, token: Secret) -> Self {
        Self {
            url: url.into(),
            token,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl fmt::Debug for ForgejoConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgejoConfig")
            .field("url", &self.url)
            .field("token", &self.token)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The account a token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Viewer {
    /// The login name.
    pub login: String,
    /// The account's email, when the instance reports one.
    #[serde(default)]
    pub email: Option<String>,
    /// The account's own description, when the instance reports one. The tool writes
    /// a mark here on an identity it creates, so a later run can tell its own machine
    /// identity from a person who happens to have that name.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the account is an administrator of the instance.
    #[serde(default)]
    pub is_admin: bool,
}

/// What one caller may do with one repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Permissions {
    /// Whether the caller administers the repository.
    #[serde(default)]
    pub admin: bool,
    /// Whether the caller may push to it.
    #[serde(default)]
    pub push: bool,
    /// Whether the caller may read it.
    #[serde(default)]
    pub pull: bool,
}

impl Permissions {
    /// The permission as one word, strongest first.
    pub fn word(&self) -> &'static str {
        if self.admin {
            "admin"
        } else if self.push {
            "write"
        } else if self.pull {
            "read"
        } else {
            "none"
        }
    }
}

/// One repository, as the token sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// `namespace/name`.
    pub full_name: String,
    /// The namespace's login (the namespace).
    pub namespace: String,
    /// The repository's own name.
    pub name: String,
    /// Whether the repository is private.
    pub private: bool,
    /// The default branch, when the instance names one.
    pub default_branch: Option<String>,
    /// The web address, when the instance gives one.
    pub html_url: Option<String>,
    /// What the caller may do with it.
    pub permissions: Permissions,
}

impl Repository {
    /// `namespace/name`, the way a person writes it.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }

    /// The one line the Forgejo screen shows for this repository.
    pub fn line(&self) -> String {
        format!(
            "{} [{}] {} — you may {}",
            self.full_name,
            if self.private { "private" } else { "public" },
            self.namespace,
            self.permissions.word(),
        )
    }
}

/// One pull request, as the instance reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// The instance's number for it.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    /// The web address, when the instance gives one.
    pub html_url: Option<String>,
    /// The head branch.
    pub head: String,
    /// The base branch.
    pub base: String,
}

impl PullRequest {
    /// One line for a screen.
    pub fn line(&self) -> String {
        format!(
            "#{} [{}] {} ({} -> {})",
            self.number, self.state, self.title, self.head, self.base
        )
    }
}

/// The result of the role check: who the token is, and what it can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCheck {
    /// The instance that was asked.
    pub url: String,
    /// The role the configuration expects the token to have.
    pub role: Role,
    /// The account the token belongs to.
    pub viewer: Viewer,
    /// The repositories the token can see.
    pub repositories: Vec<Repository>,
    /// How many of those are private: the visible difference between the
    /// administrative view and an ordinary one.
    pub private_visible: usize,
}

impl RoleCheck {
    /// Whether this is the administrative view.
    pub fn administrative(&self) -> bool {
        self.role == Role::Admin
    }

    /// One line for the status bar.
    ///
    /// The role comes first, so a narrow footer clips the counts rather than the word
    /// that says which view this is.
    pub fn summary(&self) -> String {
        // The word for the view leads, so a narrow footer clips the counts rather than
        // the word that says which view this is; the counts are compact so the checked
        // panel still gives the repository list more than one row at 80×24 even when
        // the token belongs to an administrator.
        let mut text = format!(
            "as {}, {} sees {}, {} private",
            if self.administrative() {
                "an administrator"
            } else {
                "an ordinary member"
            },
            self.viewer.login,
            words::counted(self.repositories.len(), "repository", "repositories"),
            self.private_visible,
        );
        // The label comes from the configured role, but the token may belong to an
        // administrator. Saying only "ordinary member" beside an administrator's name
        // is a contradiction the user cannot resolve; say what the token actually is
        // and name the key that shows the other view (W-8).
        if self.viewer.is_admin && !self.administrative() {
            text.push_str(&format!(
                "; `{}` is an administrator — press C for that view",
                self.viewer.login
            ));
        }
        text
    }
}

/// One repository's state, as the tool offers to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryState {
    /// The repository itself.
    pub repository: Repository,
    /// Its open pull requests.
    pub open_pull_requests: Vec<PullRequest>,
    /// Its branches.
    pub branches: Vec<String>,
}

impl RepositoryState {
    /// A few lines for a screen.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} ({}), default branch {}",
            self.repository.full_name,
            if self.repository.private {
                "private"
            } else {
                "public"
            },
            self.repository
                .default_branch
                .as_deref()
                .unwrap_or("unknown"),
        )];
        lines.push(format!(
            "{}: {}",
            words::counted(self.branches.len(), "branch", "branches"),
            if self.branches.is_empty() {
                "none".to_owned()
            } else {
                self.branches.join(", ")
            }
        ));
        lines.push(words::counted(
            self.open_pull_requests.len(),
            "open pull request",
            "open pull requests",
        ));
        for pull in &self.open_pull_requests {
            lines.push(format!("  {}", pull.line()));
        }
        lines
    }
}

/// Why a Forgejo request did not produce an answer.
#[derive(Debug, thiserror::Error)]
pub enum ForgejoError {
    /// The URL is not a usable base URL.
    #[error("the Forgejo URL {url:?} is not usable: {problem}")]
    BadUrl {
        /// The URL as configured.
        url: String,
        /// What is wrong with it.
        problem: String,
    },
    /// No token was available at all.
    #[error("no Forgejo token is configured; put one in the credentials file")]
    MissingToken,
    /// The instance refused the token.
    #[error("the Forgejo refused the token (HTTP {status})")]
    BadToken {
        /// The status the instance answered with.
        status: u16,
    },
    /// The token belongs to an ordinary account, but the configuration says admin.
    #[error(
        "the token belongs to `{login}`, who is not an administrator, but the configuration says role = \"admin\""
    )]
    NotAdmin {
        /// The account the token belongs to.
        login: String,
    },
    /// The token does not have the rights this operation needs.
    #[error("the token is not allowed to do this (HTTP {status}): {message}")]
    Forbidden {
        /// The status the instance answered with.
        status: u16,
        /// A bounded, redacted excerpt of what the instance said.
        message: String,
    },
    /// The thing asked for is not there, or the token may not see it.
    #[error("the Forgejo has nothing at {what} (HTTP 404)")]
    NotFound {
        /// The path that was asked for.
        what: String,
    },
    /// The instance answered with an error of its own.
    #[error("the Forgejo answered HTTP {status}: {message}")]
    Server {
        /// The status the instance answered with.
        status: u16,
        /// A bounded, redacted excerpt of what it said.
        message: String,
    },
    /// The instance answered, but not with what was expected.
    #[error("the Forgejo reply is not usable: {problem}")]
    MalformedReply {
        /// What was wrong with it.
        problem: String,
    },
    /// The request ran out of time.
    #[error("the Forgejo did not answer within {secs} s")]
    Timeout {
        /// How long the request was allowed.
        secs: u64,
    },
    /// The request could not be sent at all.
    #[error("the request could not be sent: {message}")]
    Transport {
        /// What the transport said.
        message: String,
    },
}

impl ForgejoError {
    /// The class of failure, as one word a screen can hold.
    pub fn class(&self) -> &'static str {
        match self {
            ForgejoError::BadUrl { .. } => "bad_url",
            ForgejoError::MissingToken => "missing_token",
            ForgejoError::BadToken { .. } => "bad_token",
            ForgejoError::NotAdmin { .. } => "not_admin",
            ForgejoError::Forbidden { .. } => "forbidden",
            ForgejoError::NotFound { .. } => "not_found",
            ForgejoError::Server { .. } => "server_error",
            ForgejoError::MalformedReply { .. } => "malformed_reply",
            ForgejoError::Timeout { .. } => "timeout",
            ForgejoError::Transport { .. } => "transport",
        }
    }
}

/// A client for one Forgejo, with one token.
pub struct ForgejoClient {
    base: String,
    token: Secret,
    agent: ureq::Agent,
}

impl ForgejoClient {
    /// Build a client, refusing a URL that is not a base URL.
    pub fn new(url: &str, token: Secret, timeout: Duration) -> Result<Self, ForgejoError> {
        let base = normalise_url(url)?;
        if token.is_empty() {
            return Err(ForgejoError::MissingToken);
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent("gewu-multiverse")
            .build()
            .new_agent();
        Ok(Self { base, token, agent })
    }

    /// The base URL, without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The `Authorization` header. The one place a token value leaves this module.
    fn authorization(&self) -> String {
        format!("token {}", self.token.expose())
    }

    /// Who the token belongs to.
    pub fn viewer(&self) -> Result<Viewer, ForgejoError> {
        let body = self.get("/api/v1/user")?;
        parse(&body, "/api/v1/user")
    }

    /// The repositories the token can see, with the caller's permission on each.
    ///
    /// `private=true` is what makes the administrative view visibly different: an
    /// administrator's token sees every repository including the private ones it does
    /// not own, while an ordinary token sees only what it may read.
    pub fn repositories(&self) -> Result<Vec<Repository>, ForgejoError> {
        let path = format!("/api/v1/repos/search?limit={MAX_REPOSITORIES}&private=true");
        let body = self.get(&path)?;
        let wire: WireSearch = parse(&body, &path)?;
        Ok(wire.data.into_iter().map(Repository::from).collect())
    }

    /// One repository, when the token may see it. `None` when it is not there.
    pub fn repository(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<Repository>, ForgejoError> {
        let path = format!("/api/v1/repos/{}/{}", segment(namespace), segment(name));
        match self.get(&path) {
            Ok(body) => Ok(Some(parse::<WireRepo>(&body, &path)?.into())),
            Err(ForgejoError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// One repository's state: the repository, its branches and its open pulls.
    pub fn repository_state(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<RepositoryState, ForgejoError> {
        let repository =
            self.repository(namespace, name)?
                .ok_or_else(|| ForgejoError::NotFound {
                    what: format!("{namespace}/{name}"),
                })?;
        let branches = self.branches(namespace, name)?;
        let open_pull_requests = self.pull_requests(namespace, name)?;
        Ok(RepositoryState {
            repository,
            open_pull_requests,
            branches,
        })
    }

    /// The branches of one repository.
    pub fn branches(&self, namespace: &str, name: &str) -> Result<Vec<String>, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/branches?limit=200",
            segment(namespace),
            segment(name)
        );
        let body = self.get(&path)?;
        let wire: Vec<WireBranch> = parse(&body, &path)?;
        Ok(wire.into_iter().map(|branch| branch.name).collect())
    }

    /// The open pull requests of one repository.
    pub fn pull_requests(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<PullRequest>, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/pulls?state=open&limit=50",
            segment(namespace),
            segment(name)
        );
        let body = self.get(&path)?;
        let wire: Vec<WirePull> = parse(&body, &path)?;
        Ok(wire.into_iter().map(PullRequest::from).collect())
    }

    /// Who an account is, when the token may see it.
    pub fn user(&self, login: &str) -> Result<Option<Viewer>, ForgejoError> {
        let path = format!("/api/v1/users/{}", segment(login));
        match self.get(&path) {
            Ok(body) => Ok(Some(parse::<Viewer>(&body, &path)?)),
            Err(ForgejoError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Create an account, as an administrator.
    pub fn create_user(
        &self,
        login: &str,
        email: &str,
        password: &str,
    ) -> Result<Viewer, ForgejoError> {
        let path = "/api/v1/admin/users";
        let body = serde_json::json!({
            "username": login,
            "email": email,
            "password": password,
            "must_change_password": false,
        })
        .to_string();
        let reply = self.post(path, &body)?;
        parse(&reply, path)
    }

    /// Write the tool's mark on an identity it created, as an administrator.
    pub fn mark_identity(&self, login: &str, full_name: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/admin/users/{}", segment(login));
        let body = serde_json::json!({
            "description": IDENTITY_DESCRIPTION,
            "full_name": full_name,
        })
        .to_string();
        self.patch(&path, &body)?;
        Ok(())
    }

    /// Create an access token for an account, as an administrator.
    ///
    /// This is why an administrator token is what lets the tool manage identities:
    /// the administrator makes the account and then makes a token for it, and only
    /// the token is kept. Nothing about the account's password is stored or written
    /// down.
    ///
    /// The request body names the rights the token is made with. That field's name is
    /// the Forgejo API's own wire key (an external protocol name, not a term of this
    /// project), and the instance refuses a token request that leaves it out.
    pub fn create_token_for(&self, login: &str, name: &str) -> Result<Secret, ForgejoError> {
        let path = format!("/api/v1/admin/users/{}/tokens", segment(login));
        let body = serde_json::json!({
            "name": name,
            "scopes": ["write:repository", "write:user", "read:user", "read:repository"],
        })
        .to_string();
        let reply = self.post(&path, &body)?;
        let wire: WireToken = parse(&reply, &path)?;
        Ok(Secret::new(wire.sha1))
    }

    /// Open a pull request, as the token's account.
    pub fn open_pull_request(
        &self,
        namespace: &str,
        name: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/pulls",
            segment(namespace),
            segment(name)
        );
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "head": head,
            "base": base,
        })
        .to_string();
        let reply = self.post(&path, &payload)?;
        Ok(parse::<WirePull>(&reply, &path)?.into())
    }

    /// Update a pull request's title, body or state, as the token's account.
    pub fn update_pull_request(
        &self,
        namespace: &str,
        name: &str,
        number: u64,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<&str>,
    ) -> Result<PullRequest, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/pulls/{number}",
            segment(namespace),
            segment(name)
        );
        let mut payload = serde_json::Map::new();
        if let Some(title) = title {
            payload.insert("title".to_owned(), serde_json::json!(title));
        }
        if let Some(body) = body {
            payload.insert("body".to_owned(), serde_json::json!(body));
        }
        if let Some(state) = state {
            payload.insert("state".to_owned(), serde_json::json!(state));
        }
        let reply = self.patch(&path, &serde_json::Value::Object(payload).to_string())?;
        Ok(parse::<WirePull>(&reply, &path)?.into())
    }

    /// `GET` a path and return its body, or a classified failure.
    fn get(&self, path: &str) -> Result<String, ForgejoError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .get(&url)
            .header("Accept", "application/json")
            .header("Authorization", self.authorization())
            .call();
        self.finish(response, path)
    }

    /// `POST` a JSON body and return the reply, or a classified failure.
    fn post(&self, path: &str, body: &str) -> Result<String, ForgejoError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .post(&url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Authorization", self.authorization())
            .send(body);
        self.finish(response, path)
    }

    /// `PATCH` a JSON body and return the reply, or a classified failure.
    fn patch(&self, path: &str, body: &str) -> Result<String, ForgejoError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .patch(&url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Authorization", self.authorization())
            .send(body);
        self.finish(response, path)
    }

    /// Turn a transport result into a status and a body, or a classified failure.
    fn finish(
        &self,
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        path: &str,
    ) -> Result<String, ForgejoError> {
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Timeout(_)) => {
                return Err(ForgejoError::Timeout { secs: 30 });
            }
            Err(ureq::Error::BadUri(problem)) => {
                return Err(ForgejoError::BadUrl {
                    url: self.base.clone(),
                    problem,
                });
            }
            Err(other) => {
                return Err(ForgejoError::Transport {
                    message: self.token.redact(&other.to_string()),
                });
            }
        };
        let status = response.status().as_u16();
        let text =
            response
                .into_body()
                .read_to_string()
                .map_err(|error| ForgejoError::Transport {
                    message: self
                        .token
                        .redact(&format!("the reply body cannot be read: {error}")),
                })?;
        let body = self.token.redact(&text);
        match status {
            401 => Err(ForgejoError::BadToken { status }),
            403 => Err(ForgejoError::Forbidden {
                status,
                message: excerpt(&body, 300),
            }),
            404 => Err(ForgejoError::NotFound {
                what: path.to_owned(),
            }),
            status if !(200..=299).contains(&status) => Err(ForgejoError::Server {
                status,
                message: excerpt(&body, 300),
            }),
            _ => Ok(body),
        }
    }
}

impl fmt::Debug for ForgejoClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgejoClient")
            .field("base", &self.base)
            .field("token", &REDACTED)
            .finish()
    }
}

/// How the role check ended, as the TUI and the tests carry it.
pub fn role_check(config: &ForgejoConfig, role: Role) -> Result<RoleCheck, ForgejoError> {
    let client = ForgejoClient::new(&config.url, config.token.clone(), config.timeout)?;
    let viewer = client.viewer()?;
    if role == Role::Admin && !viewer.is_admin {
        return Err(ForgejoError::NotAdmin {
            login: viewer.login.clone(),
        });
    }
    let repositories = client.repositories()?;
    let private_visible = repositories.iter().filter(|repo| repo.private).count();
    Ok(RoleCheck {
        url: client.base().to_owned(),
        role,
        viewer,
        repositories,
        private_visible,
    })
}

/// Check that a URL is a usable base URL and return it without a trailing slash.
pub fn normalise_url(url: &str) -> Result<String, ForgejoError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(ForgejoError::BadUrl {
            url: url.to_owned(),
            problem: "it is empty".to_owned(),
        });
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(ForgejoError::BadUrl {
            url: url.to_owned(),
            problem: "it must start with http:// or https://".to_owned(),
        });
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(ForgejoError::BadUrl {
            url: url.to_owned(),
            problem: "it holds whitespace".to_owned(),
        });
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// The host of a base URL, without the scheme or any path.
pub fn host_of(url: &str) -> String {
    let rest = url
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    rest.split('/').next().unwrap_or(rest).to_owned()
}

/// Percent-encode one path segment so a name can never change the path.
fn segment(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// Parse a JSON body, naming the path when it is not the expected shape.
fn parse<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> Result<T, ForgejoError> {
    serde_json::from_str(body).map_err(|error| ForgejoError::MalformedReply {
        problem: format!("{what} did not answer with JSON of the expected shape: {error}"),
    })
}

/// A bounded excerpt of some text, with a marker when it was cut.
fn excerpt(text: &str, limit: usize) -> String {
    crate::model::excerpt(text, limit)
}

/// The wire shape of `GET /repos/search`.
#[derive(Debug, Deserialize)]
struct WireSearch {
    #[serde(default)]
    data: Vec<WireRepo>,
}

/// The wire shape of one repository.
#[derive(Debug, Deserialize)]
struct WireRepo {
    full_name: String,
    name: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    permissions: Option<Permissions>,
}

impl From<WireRepo> for Repository {
    fn from(wire: WireRepo) -> Self {
        // The namespace is the part of `namespace/name` before the slash, so the
        // wire's own nested account object is not needed at all.
        let namespace = wire
            .full_name
            .split_once('/')
            .map(|(namespace, _)| namespace.to_owned())
            .unwrap_or_default();
        Self {
            full_name: wire.full_name,
            namespace,
            name: wire.name,
            private: wire.private,
            default_branch: wire.default_branch,
            html_url: wire.html_url,
            permissions: wire.permissions.unwrap_or_default(),
        }
    }
}

/// The wire shape of one branch.
#[derive(Debug, Deserialize)]
struct WireBranch {
    name: String,
}

/// The wire shape of one pull request.
#[derive(Debug, Deserialize)]
struct WirePull {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    html_url: Option<String>,
    head: WireRef,
    base: WireRef,
}

#[derive(Debug, Deserialize)]
struct WireRef {
    #[serde(rename = "ref")]
    reference: String,
}

impl From<WirePull> for PullRequest {
    fn from(wire: WirePull) -> Self {
        Self {
            number: wire.number,
            title: wire.title,
            state: wire.state,
            html_url: wire.html_url,
            head: wire.head.reference,
            base: wire.base.reference,
        }
    }
}

/// The wire shape of a token that was just created.
#[derive(Debug, Deserialize, Serialize)]
struct WireToken {
    #[serde(default)]
    sha1: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_administrators_token_checked_as_a_member_is_not_called_a_member() {
        // W-8: the label comes from the configured role, but the token may be an
        // administrator's; the summary must not call the administrator an ordinary
        // member without a word about the contradiction.
        let check = RoleCheck {
            url: "https://forgejo.example.org".to_owned(),
            role: crate::config::Role::Member,
            viewer: Viewer {
                login: "uxadmin".to_owned(),
                email: None,
                description: None,
                is_admin: true,
            },
            repositories: Vec::new(),
            private_visible: 0,
        };
        let summary = check.summary();
        assert!(summary.contains("administrator"), "{summary}");
        assert!(summary.contains("press C"), "{summary}");
    }

    #[test]
    fn a_url_without_a_scheme_is_refused() {
        let error = normalise_url("forgejo.example.org").expect_err("refused");
        assert_eq!(error.class(), "bad_url");
        assert!(error.to_string().contains("https://"));
    }

    #[test]
    fn a_trailing_slash_is_removed() {
        assert_eq!(
            normalise_url("https://forgejo.example.org/").expect("accepted"),
            "https://forgejo.example.org"
        );
        assert_eq!(
            host_of("https://forgejo.example.org/sub/path"),
            "forgejo.example.org"
        );
    }

    #[test]
    fn an_empty_token_is_refused_before_any_request() {
        let error = ForgejoClient::new(
            "https://forgejo.example.org",
            Secret::empty(),
            DEFAULT_TIMEOUT,
        )
        .expect_err("refused");
        assert_eq!(error.class(), "missing_token");
    }

    #[test]
    fn a_client_never_prints_its_token() {
        let client = ForgejoClient::new(
            "https://forgejo.example.org",
            Secret::new("gw-fixture-token-not-real"),
            DEFAULT_TIMEOUT,
        )
        .expect("a client");
        let printed = format!("{client:?} {client:?}");
        assert!(!printed.contains("gw-fixture-token"), "{printed}");
        assert!(printed.contains(REDACTED), "{printed}");
    }

    #[test]
    fn a_permission_is_one_word_strongest_first() {
        assert_eq!(Permissions::default().word(), "none");
        assert_eq!(
            Permissions {
                pull: true,
                ..Permissions::default()
            }
            .word(),
            "read"
        );
        assert_eq!(
            Permissions {
                push: true,
                pull: true,
                ..Permissions::default()
            }
            .word(),
            "write"
        );
        assert_eq!(
            Permissions {
                admin: true,
                push: true,
                pull: true
            }
            .word(),
            "admin"
        );
    }

    #[test]
    fn a_name_that_holds_a_separator_is_encoded_so_it_cannot_change_the_path() {
        assert_eq!(segment("namespace"), "namespace");
        assert_eq!(segment("a/b"), "a%2Fb");
        assert_eq!(segment("a b"), "a%20b");
    }

    #[test]
    fn a_repository_line_names_namespace_visibility_and_permission() {
        let repo = Repository {
            full_name: "root/demo".to_owned(),
            namespace: "root".to_owned(),
            name: "demo".to_owned(),
            private: true,
            default_branch: Some("main".to_owned()),
            html_url: None,
            permissions: Permissions {
                push: true,
                pull: true,
                ..Permissions::default()
            },
        };
        let line = repo.line();
        assert!(line.contains("root/demo"), "{line}");
        assert!(line.contains("private"), "{line}");
        assert!(line.contains("write"), "{line}");
    }

    #[test]
    fn the_role_check_summary_leads_with_the_role() {
        // F-F: a narrow footer must clip the counts, never the word that says which
        // view this is. The private count appears once, not twice.
        let check = RoleCheck {
            url: "https://forgejo.example.org".to_owned(),
            role: Role::Admin,
            viewer: Viewer {
                login: "root".to_owned(),
                email: None,
                description: None,
                is_admin: true,
            },
            repositories: Vec::new(),
            private_visible: 3,
        };
        assert_eq!(
            check.summary(),
            "as an administrator, root sees 0 repositories, 3 private"
        );
        assert_eq!(check.summary().matches("private").count(), 1);
    }
}
