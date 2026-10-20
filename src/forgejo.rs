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

use std::cell::Cell;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::credentials::{REDACTED, Secret};

/// How long one Forgejo request may take.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many repositories one listing asks for at most.
pub const MAX_REPOSITORIES: usize = 200;

/// How many assigned issues one search may return.
pub const MAX_ISSUES: usize = 50;

/// How many comments one read may return.
pub const MAX_COMMENTS: usize = 20;

/// The name the tool gives an agent identity, before the agent's own name.
pub const LOGIN_PREFIX: &str = "gewu-";

/// The issue right a read needs.
pub const ISSUE_READ_SCOPE: &str = "read:issue";

/// The issue right a write needs.
pub const ISSUE_WRITE_SCOPE: &str = "write:issue";

/// The rights a managed agent token is made with: repository and user rights for the
/// clone and the identity, and issue rights for the tools. A token made before the
/// issue tools existed carries the first four only.
pub const AGENT_TOKEN_SCOPES: [&str; 6] = [
    "write:repository",
    "write:user",
    "read:user",
    "read:repository",
    ISSUE_READ_SCOPE,
    ISSUE_WRITE_SCOPE,
];

/// The description the tool writes on an identity it creates, so a later run can
/// tell its own machine identity from a person who happens to have that name.
pub const IDENTITY_DESCRIPTION: &str = "Machine identity created by gewu-multiverse.";

/// The domain agent identities use for their Forgejo email address. `.invalid` is
/// reserved and can never resolve, so a commit authored with it cannot be mistaken
/// for mail.
pub const IDENTITY_EMAIL_DOMAIN: &str = "agents.invalid";

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
    /// Whether the account may sign in. A removed agent's account is kept with
    /// `active = false`, so its rows and history stay and its sign-in does not.
    #[serde(default = "default_active")]
    pub active: bool,
    /// Whether sign-in is explicitly prohibited. Removal sets this as well as
    /// `active = false`, because Forgejo otherwise signs the account in and then
    /// shows it as suspended.
    #[serde(default)]
    pub prohibit_login: bool,
}

fn default_active() -> bool {
    true
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
    /// The head repository's full name, when the instance names it.
    pub head_repo: Option<String>,
    /// The base branch.
    pub base: String,
    /// The base repository's full name, when the instance names it.
    pub base_repo: Option<String>,
    /// The login that opened the request, when the instance names it.
    pub author: Option<String>,
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

/// One issue, as the instance reports it. A pull request is also an issue, and its
/// `pull_request` marker says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The instance's number for it.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Its body, when it has one.
    pub body: Option<String>,
    /// `open`, `closed`.
    pub state: String,
    /// The web address, when the instance gives one.
    pub html_url: Option<String>,
    /// Its labels' names.
    pub labels: Vec<String>,
    /// When it was last updated, as the instance writes it.
    pub updated_at: Option<String>,
    /// The login that opened it, when the instance names one.
    pub author: Option<String>,
    /// The number of comments the instance counts.
    pub comments: u64,
    /// `owner/name`, when the reply names the repository.
    pub repository: Option<String>,
    /// The pull-request marker's web address, when this issue is a pull request.
    pub pull_request_url: Option<String>,
}

impl Issue {
    /// `owner/repo#number`, the way a person writes it.
    pub fn reference(&self, repository: &str) -> String {
        format!("{repository}#{}", self.number)
    }
}

/// One comment on an issue or pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The instance's id for it.
    pub id: u64,
    /// Its text.
    pub body: String,
    /// The web address, when the instance gives one.
    pub html_url: Option<String>,
    /// The login that wrote it, when the instance names one.
    pub author: Option<String>,
    /// When it was written, as the instance writes it.
    pub created_at: Option<String>,
}

/// One team of an organisation, as far as the bundled engine needs it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Team {
    /// The instance's id for the team, used to add and remove members.
    pub id: u64,
    /// The team's name, e.g. `agents`.
    pub name: String,
    /// `read`, `write` or `admin`.
    #[serde(default)]
    pub permission: String,
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
    /// The token does not have the rights this operation needs.
    #[error("the token is not allowed to do this (HTTP {status}): {message}")]
    Forbidden {
        /// The status the instance answered with.
        status: u16,
        /// A bounded, redacted excerpt of what the instance said.
        message: String,
    },
    /// The token does not carry the scope the operation needs.
    ///
    /// Forgejo names the missing right in the refusal body; that name is kept, and
    /// the rest of the body is never carried forward. An empty list means the
    /// instance refused for scope reasons but named no right.
    #[error("the Forgejo token is missing the right(s) {} (HTTP {status})", rights.join(", "))]
    MissingScope {
        /// The status the instance answered with.
        status: u16,
        /// The rights the instance named as missing.
        rights: Vec<String>,
    },
    /// The thing asked for is not there, or the token may not see it.
    #[error("the Forgejo has nothing at {what} (HTTP 404){hint}")]
    NotFound {
        /// The path that was asked for.
        what: String,
        /// A correction when the configured base URL carries a page path.
        hint: String,
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
            ForgejoError::Forbidden { .. } => "forbidden",
            ForgejoError::MissingScope { .. } => "missing_scope",
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
    /// How long one request may take, so a timeout message names the real bound
    /// instead of a second hard-coded number (N11).
    timeout: Duration,
    /// Whether the newest request that produced an answer was answered with a 2xx.
    ///
    /// A write can be accepted by the instance and still carry a body this client
    /// cannot read as JSON (a proxy truncation, another reply shape). The write
    /// nevertheless happened, so the tools count it against their bound from this
    /// flag rather than from whether the reply parsed. [`ForgejoClient::begin_write`]
    /// clears it before a write, so an earlier read's 2xx can never be mistaken for a
    /// write's.
    last_2xx: Cell<bool>,
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
        Ok(Self {
            base,
            token,
            agent,
            timeout,
            last_2xx: Cell::new(false),
        })
    }

    /// Note that a request is about to be a write, so an earlier read's 2xx cannot
    /// stand in for the write's answer.
    pub fn begin_write(&self) {
        self.last_2xx.set(false);
    }

    /// Whether the newest answered request was a 2xx. A transport failure leaves the
    /// flag as it was, so callers clear it first with [`ForgejoClient::begin_write`].
    pub fn last_2xx(&self) -> bool {
        self.last_2xx.get()
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

    /// One issue or pull request by number, with its own facts and comment count.
    pub fn issue(&self, namespace: &str, name: &str, number: u64) -> Result<Issue, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{number}",
            segment(namespace),
            segment(name)
        );
        let body = self.get(&path)?;
        let mut issue: Issue = parse::<WireIssue>(&body, &path)?.into();
        // The issue reply does not repeat the repository; the caller's request does.
        issue.repository = Some(format!("{namespace}/{name}"));
        Ok(issue)
    }

    /// Open one issue, as the token's account. Used by the bundled engine's tests and
    /// by a person-facing setup step to leave one piece of work for an agent.
    pub fn create_issue(
        &self,
        namespace: &str,
        name: &str,
        title: &str,
        body: &str,
    ) -> Result<Issue, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues",
            segment(namespace),
            segment(name)
        );
        let payload = serde_json::json!({ "title": title, "body": body }).to_string();
        let reply = self.post(&path, &payload)?;
        let mut issue: Issue = parse::<WireIssue>(&reply, &path)?.into();
        issue.repository = Some(format!("{namespace}/{name}"));
        Ok(issue)
    }

    /// The open issues assigned to the token's own account, across every repository
    /// the token can see. The caller narrows them to its cloned repositories.
    pub fn assigned_issues(&self) -> Result<Vec<Issue>, ForgejoError> {
        let path = format!(
            "/api/v1/repos/issues/search?state=open&assigned=true&type=issues&limit={}",
            MAX_ISSUES
        );
        let body = self.get(&path)?;
        let wire: Vec<WireIssue> = parse(&body, &path)?;
        Ok(wire.into_iter().map(Issue::from).collect())
    }

    /// The comments of one issue or pull request, oldest first, bounded.
    pub fn issue_comments(
        &self,
        namespace: &str,
        name: &str,
        number: u64,
        limit: usize,
    ) -> Result<Vec<Comment>, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{number}/comments?limit={}",
            segment(namespace),
            segment(name),
            limit.min(MAX_COMMENTS)
        );
        let body = self.get(&path)?;
        let wire: Vec<WireComment> = parse(&body, &path)?;
        Ok(wire.into_iter().map(Comment::from).collect())
    }

    /// Write one comment on an issue or pull request, as the token's account.
    pub fn comment_on_issue(
        &self,
        namespace: &str,
        name: &str,
        number: u64,
        body: &str,
    ) -> Result<Comment, ForgejoError> {
        let path = format!(
            "/api/v1/repos/{}/{}/issues/{number}/comments",
            segment(namespace),
            segment(name)
        );
        let reply = self.post(&path, &serde_json::json!({ "body": body }).to_string())?;
        Ok(parse::<WireComment>(&reply, &path)?.into())
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

    /// Delete one repository, as an administrator. Used by the confinement proof:
    /// an agent's own token must be refused this call.
    pub fn delete_repository(&self, namespace: &str, name: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/repos/{}/{}", segment(namespace), segment(name));
        self.delete(&path)
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

    /// Deactivate an account, as an administrator.
    ///
    /// The row, its commits, its issues and its comments stay; `active = false` is
    /// the deactivation the walk asked for, and `prohibit_login = true` is what makes
    /// Forgejo refuse a web sign-in instead of signing the account in and then saying
    /// it is suspended. This is removal in the product: history is not rewritten.
    pub fn deactivate_user(&self, login: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/admin/users/{}", segment(login));
        let body = serde_json::json!({ "active": false, "prohibit_login": true }).to_string();
        self.patch(&path, &body)?;
        Ok(())
    }

    /// Set whether an account may sign in. Reactivating is how an agent removed and
    /// then added again gets its account back without losing its history.
    pub fn set_user_active(&self, login: &str, active: bool) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/admin/users/{}", segment(login));
        let body = serde_json::json!({ "active": active, "prohibit_login": !active }).to_string();
        self.patch(&path, &body)?;
        Ok(())
    }

    /// Set an account's password, as an administrator, through the API.
    ///
    /// The value travels in the `PATCH` body; it is never a process argument, so an
    /// ordinary process table cannot show it. This is the whole of `p`.
    pub fn set_user_password(&self, login: &str, password: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/admin/users/{}", segment(login));
        let body = serde_json::json!({
            "password": password,
            "must_change_password": false,
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
            "scopes": AGENT_TOKEN_SCOPES,
        })
        .to_string();
        let reply = self.post(&path, &body)?;
        let wire: WireToken = parse(&reply, &path)?;
        Ok(Secret::new(wire.sha1))
    }

    /// Delete an access token of an account, as an administrator.
    ///
    /// This is how a renewal revokes the credential it replaces rather than merely
    /// leaving it behind: the name recorded when the token was made names the key to
    /// remove. A token that is already gone is not an error, because the thing the
    /// caller wanted — that token no longer answers — is already true.
    pub fn delete_token_for(&self, login: &str, name: &str) -> Result<bool, ForgejoError> {
        let path = format!(
            "/api/v1/admin/users/{}/tokens/{}",
            segment(login),
            segment(name)
        );
        match self.delete(&path) {
            Ok(()) => Ok(true),
            Err(ForgejoError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error),
        }
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

    /// Create the one organisation, treating an existing name as success.
    pub fn ensure_organisation(&self, name: &str) -> Result<(), ForgejoError> {
        let path = "/api/v1/orgs";
        let body = serde_json::json!({
            "username": name,
            "full_name": name,
            "visibility": "private",
        })
        .to_string();
        match self.post(path, &body) {
            Ok(_) => Ok(()),
            Err(ForgejoError::Server { status, message })
                if (status == 409 || status == 422) && message.contains("already exists") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The teams of one organisation.
    pub fn teams(&self, organisation: &str) -> Result<Vec<Team>, ForgejoError> {
        let path = format!("/api/v1/orgs/{}/teams", segment(organisation));
        let body = self.get(&path)?;
        let wire: Vec<WireTeam> = parse(&body, &path)?;
        Ok(wire.into_iter().map(Team::from).collect())
    }

    /// The team named `name` in one organisation, creating it with write permission
    /// over all the organisation's repositories when it is not there yet.
    pub fn ensure_team(&self, organisation: &str, name: &str) -> Result<Team, ForgejoError> {
        if let Some(team) = self
            .teams(organisation)?
            .into_iter()
            .find(|team| team.name == name)
        {
            return Ok(team);
        }
        let path = format!("/api/v1/orgs/{}/teams", segment(organisation));
        let body = serde_json::json!({
            "name": name,
            "description": "Agents of gewu-multiverse",
            "permission": "write",
            "includes_all_repositories": true,
            "can_create_org_repo": false,
            // Forgejo requires the unit permissions to be named; `write` alone is not
            // enough for a team that must comment, open pull requests and push code.
            // Forgejo derives the team's summary permission from the *minimum* unit,
            // so only the units the agent tools need are named, all with write. A
            // read-only unit here would make the whole team read.
            "units": [
                "repo.code",
                "repo.issues",
                "repo.pulls",
                "repo.projects",
            ],
            "units_map": {
                "repo.code": "write",
                "repo.issues": "write",
                "repo.pulls": "write",
                "repo.projects": "write",
            },
        })
        .to_string();
        let reply = match self.post(&path, &body) {
            Ok(reply) => reply,
            // A second program cannot be here (the data lock forbids it), but a
            // retried request after a lost reply can be: read the team it made.
            Err(ForgejoError::Server { status: 409, .. }) => {
                return self
                    .teams(organisation)?
                    .into_iter()
                    .find(|team| team.name == name)
                    .ok_or_else(|| ForgejoError::Server {
                        status: 409,
                        message: "the team exists but could not be read".to_owned(),
                    });
            }
            Err(error) => return Err(error),
        };
        Ok(parse::<WireTeam>(&reply, &path)?.into())
    }

    /// The members of one team.
    pub fn team_members(&self, team_id: u64) -> Result<Vec<Viewer>, ForgejoError> {
        let path = format!("/api/v1/teams/{team_id}/members");
        let body = self.get(&path)?;
        parse(&body, &path)
    }

    /// Add an account to a team. Adding a member twice is not an error.
    pub fn add_team_member(&self, team_id: u64, login: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/teams/{team_id}/members/{}", segment(login));
        let response = self
            .agent
            .put(&format!("{}{path}", self.base))
            .header("Accept", "application/json")
            .header("Authorization", self.authorization())
            .send_empty();
        match self.finish(response, &path) {
            Ok(_) => Ok(()),
            // The account is already a member: the state the caller asked for.
            Err(ForgejoError::Server { status, .. }) if status == 409 || status == 422 => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Remove an account from a team. An account that is not a member is not an error.
    pub fn remove_team_member(&self, team_id: u64, login: &str) -> Result<(), ForgejoError> {
        let path = format!("/api/v1/teams/{team_id}/members/{}", segment(login));
        match self.delete(&path) {
            Ok(()) | Err(ForgejoError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The repositories of one organisation, as the token sees them.
    pub fn organisation_repositories(
        &self,
        organisation: &str,
    ) -> Result<Vec<Repository>, ForgejoError> {
        let path = format!("/api/v1/orgs/{}/repos?limit=50", segment(organisation));
        let body = self.get(&path)?;
        let wire: Vec<WireRepo> = parse(&body, &path)?;
        Ok(wire.into_iter().map(Repository::from).collect())
    }

    /// Create the organisation's starter repository, initialised with a README.
    /// An existing repository is not an error.
    pub fn ensure_organisation_repository(
        &self,
        organisation: &str,
        name: &str,
    ) -> Result<Repository, ForgejoError> {
        let path = format!("/api/v1/orgs/{}/repos", segment(organisation));
        let body = serde_json::json!({
            "name": name,
            "description": "The agents' shared work",
            "private": false,
            "auto_init": true,
            "default_branch": "main",
        })
        .to_string();
        match self.post(&path, &body) {
            Ok(reply) => Ok(parse::<WireRepo>(&reply, &path)?.into()),
            Err(ForgejoError::Server { status, .. }) if status == 409 || status == 422 => self
                .repository(organisation, name)?
                .ok_or_else(|| ForgejoError::Server {
                    status,
                    message: "the repository exists but could not be read".to_owned(),
                }),
            Err(error) => Err(error),
        }
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

    /// `DELETE` a path. The reply's body is not used, but its status is classified.
    fn delete(&self, path: &str) -> Result<(), ForgejoError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .delete(&url)
            .header("Accept", "application/json")
            .header("Authorization", self.authorization())
            .call();
        self.finish(response, path).map(|_| ())
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
                return Err(ForgejoError::Timeout {
                    secs: self.timeout.as_secs().max(1),
                });
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
        // Ask first whether the instance accepted the request, before naming any
        // failure: a 2xx whose body this client cannot parse is still an accepted
        // write, and the tools' bound must count it (B2).
        self.last_2xx.set((200..=299).contains(&status));
        match status {
            401 => Err(ForgejoError::BadToken { status }),
            403 => match missing_scopes(&body) {
                Some(rights) => Err(ForgejoError::MissingScope { status, rights }),
                None => Err(ForgejoError::Forbidden {
                    status,
                    message: reply_message(&body, 300),
                }),
            },
            404 => Err(ForgejoError::NotFound {
                what: path.to_owned(),
                hint: page_path_hint(&self.base),
            }),
            status if !(200..=299).contains(&status) => Err(ForgejoError::Server {
                status,
                message: reply_message(&body, 300),
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

/// A correction when a configured base URL includes a page path.
///
/// A browser address such as `https://forgejo.example.org/explore/repos` is a page,
/// not the instance root the API lives under. The failure says that plainly, with
/// the root it can see, instead of only naming an API path the person never typed.
fn page_path_hint(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => return String::new(),
    };
    let Some((host, path)) = rest.split_once('/') else {
        return String::new();
    };
    if host.is_empty() || path.is_empty() {
        return String::new();
    }
    format!(
        " — {base} includes the page path /{path}; use the instance root {scheme}://{host} instead"
    )
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
/// The rights Forgejo named as missing in a scope refusal, when it named any.
///
/// Forgejo answers `403` with a body like
/// `{"message":"token does not have at least one of required scope(s): [read:issue]"}`.
/// Only the bracketed names are kept; the rest of the body is not carried forward.
fn missing_scopes(body: &str) -> Option<Vec<String>> {
    let marker = "required scope(s):";
    let after = &body[body.find(marker)? + marker.len()..];
    let open = after.find('[')?;
    let close = after[open + 1..].find(']')?;
    let rights: Vec<String> = after[open + 1..open + 1 + close]
        .split(',')
        .map(str::trim)
        .filter(|right| !right.is_empty())
        .map(str::to_owned)
        .collect();
    (!rights.is_empty()).then_some(rights)
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> Result<T, ForgejoError> {
    serde_json::from_str(body).map_err(|error| ForgejoError::MalformedReply {
        problem: format!("{what} did not answer with JSON of the expected shape: {error}"),
    })
}

/// The provider's own message from an error reply, as one bounded line.
///
/// A Forgejo error envelope is read for `message`, `error.message` or a string
/// `error`; the envelope itself is never pasted into a screen or a status line. A
/// plain-text body is its own message, and a JSON body with no message leaves the
/// HTTP status as the whole fact.
fn reply_message(body: &str, limit: usize) -> String {
    let message = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
            .unwrap_or("")
            .to_owned(),
        Err(_) => body.to_owned(),
    };
    let one_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    excerpt(&one_line, limit)
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
    #[serde(default)]
    user: Option<WireUser>,
}

#[derive(Debug, Deserialize)]
struct WireLabel {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct WirePullMarker {
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    labels: Vec<WireLabel>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    repository: Option<WireRepoName>,
    #[serde(default)]
    pull_request: Option<WirePullMarker>,
}

impl From<WireIssue> for Issue {
    fn from(wire: WireIssue) -> Self {
        Self {
            number: wire.number,
            title: wire.title,
            body: wire.body,
            state: wire.state,
            html_url: wire.html_url,
            labels: wire.labels.into_iter().map(|label| label.name).collect(),
            updated_at: wire.updated_at,
            author: wire.user.and_then(|user| user.login),
            comments: wire.comments,
            repository: wire.repository.and_then(|repo| repo.full_name),
            pull_request_url: wire.pull_request.and_then(|marker| marker.html_url),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireComment {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    created_at: Option<String>,
}

impl From<WireComment> for Comment {
    fn from(wire: WireComment) -> Self {
        Self {
            id: wire.id,
            body: wire.body,
            html_url: wire.html_url,
            author: wire.user.and_then(|user| user.login),
            created_at: wire.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireRef {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    repo: Option<WireRepoName>,
}

#[derive(Debug, Deserialize)]
struct WireRepoName {
    #[serde(default)]
    full_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUser {
    #[serde(default)]
    login: Option<String>,
}

impl WireRef {
    fn full_name(&self) -> Option<String> {
        self.repo.as_ref().and_then(|repo| repo.full_name.clone())
    }
}

impl From<WirePull> for PullRequest {
    fn from(wire: WirePull) -> Self {
        Self {
            number: wire.number,
            title: wire.title,
            state: wire.state,
            html_url: wire.html_url,
            head_repo: wire.head.full_name(),
            head: wire.head.reference,
            base_repo: wire.base.full_name(),
            base: wire.base.reference,
            author: wire.user.and_then(|user| user.login),
        }
    }
}

/// The wire shape of a token that was just created.
#[derive(Debug, Deserialize, Serialize)]
struct WireToken {
    #[serde(default)]
    sha1: String,
}

/// The wire shape of one team.
#[derive(Debug, Deserialize)]
struct WireTeam {
    id: u64,
    name: String,
    #[serde(default)]
    permission: String,
}

impl From<WireTeam> for Team {
    fn from(wire: WireTeam) -> Self {
        Self {
            id: wire.id,
            name: wire.name,
            permission: wire.permission,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_timed_out_call_names_the_clients_own_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener");
        let addr = listener.local_addr().expect("an address");
        std::thread::spawn(move || {
            if let Ok((_stream, _peer)) = listener.accept() {
                // Accept, answer nothing, and hold the connection past the timeout.
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        let client = ForgejoClient::new(
            &format!("http://{addr}"),
            Secret::new("gw-fixture-token-not-real"),
            Duration::from_secs(1),
        )
        .expect("a client");
        let started = std::time::Instant::now();
        let error = client.viewer().expect_err("the call times out");
        assert_eq!(error.class(), "timeout");
        let text = error.to_string();
        assert!(text.contains("within 1 s"), "{text}");
        assert!(
            !text.contains("30 s"),
            "the message names a wrong bound: {text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the client did not honour its own timeout: {:?}",
            started.elapsed()
        );
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
}
