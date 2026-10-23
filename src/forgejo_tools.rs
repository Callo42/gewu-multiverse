//! The Forgejo tools a run may declare beside `shell`.
//!
//! A run only declares these tools when the product has a configured Forgejo, the
//! agent's identity is settled with its own token, and at least one repository is
//! cloned into its workspace ([`crate::run::RunForgejo`]). Every call here is made by
//! the product with the agent's own token, read from the credentials file at the call
//! and never handed to the model. The token is not in a result, a transcript, the log
//! or `shell`'s environment.
//!
//! The rules the person's `P` and `U` keys use are the rules here: the open gate and
//! the authorship rule live in [`crate::pull_gate`] and are shared, not copied. The
//! authority is deliberately narrow — read assigned issues, read one issue or pull
//! request with its comments, comment, list the agent's own open pull requests, open a
//! pull request from the clone's pushed branch, and update the agent's own open
//! request. There is no merge, close, reopen, delete, label, assignment, review or
//! repository setting.
//!
//! Every refusal is one bounded sentence the model can act on, and every Forgejo
//! failure names its class and status, never a raw reply body.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::credentials::Secret;
use crate::forgejo::{self, ForgejoClient, ForgejoError, Issue, PullRequest};
use crate::model::MAX_TOOL_OUTPUT;
use crate::pull_gate::{self, PullRequestAuthorship};
use crate::run::{RunForgejo, RunForgejoRepository};
use crate::workspace;

/// The most writes one run may make in Forgejo.
///
/// A comment and a pull-request open or update each count; a refused or failed write
/// does not. Reads do not count and continue after the bound.
pub const WRITE_BOUND: u32 = 8;

/// The most of an issue's or pull request's body one read returns.
pub const READ_BODY_BYTES: usize = 8 * 1024;

/// The most of one comment's body one read returns.
pub const READ_COMMENT_BYTES: usize = 2 * 1024;

/// The most comments one read returns.
pub const READ_COMMENT_COUNT: usize = 20;

/// The most bytes one comment may write.
pub const WRITE_COMMENT_BYTES: usize = 8 * 1024;

/// The name of the assigned-issues tool.
pub const LIST_ASSIGNED_ISSUES: &str = "forgejo_list_assigned_issues";
/// The name of the read-one tool.
pub const READ_ISSUE_OR_PULL: &str = "forgejo_read_issue_or_pull";
/// The name of the comment tool.
pub const COMMENT: &str = "forgejo_comment";
/// The name of the own-pull-requests tool.
pub const LIST_MY_PULL_REQUESTS: &str = "forgejo_list_my_pull_requests";
/// The name of the open tool.
pub const OPEN_PULL_REQUEST: &str = "forgejo_open_pull_request";
/// The name of the update tool.
pub const UPDATE_PULL_REQUEST: &str = "forgejo_update_pull_request";

/// Every Forgejo tool, in the order they are declared.
pub const TOOL_NAMES: [&str; 6] = [
    LIST_ASSIGNED_ISSUES,
    READ_ISSUE_OR_PULL,
    COMMENT,
    LIST_MY_PULL_REQUESTS,
    OPEN_PULL_REQUEST,
    UPDATE_PULL_REQUEST,
];

/// Whether a tool writes, and so counts against [`WRITE_BOUND`].
pub fn is_write_tool(name: &str) -> bool {
    matches!(name, COMMENT | OPEN_PULL_REQUEST | UPDATE_PULL_REQUEST)
}

/// What one tool call did, for the run to record and to feed the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReply {
    /// The bounded text returned to the model.
    pub text: String,
    /// The repository the call acted on, when it named one.
    pub repository: Option<String>,
    /// The issue or pull request number, when it named one.
    pub number: Option<u64>,
    /// `commented`, `opened` or `updated` for a successful write.
    pub action: Option<&'static str>,
    /// The web URL of what a successful write changed.
    pub url: Option<String>,
    /// The clone branch a pull-request write actually used, when the tool read one.
    ///
    /// This is the per-request fact the Runs note pairs with the request number; it
    /// is never inferred from a later push record.
    pub branch: Option<String>,
}

/// What a call asked for, read before it runs so the transcript can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFacts {
    /// `owner/name`, when the arguments name one.
    pub repository: Option<String>,
    /// The number, when the arguments name one.
    pub number: Option<u64>,
    /// A short description of the act, with no secret.
    pub detail: String,
}

/// The state one run's Forgejo tools share.
pub struct ForgejoTools {
    config: RunForgejo,
    workspace: PathBuf,
    agent: String,
    token: Secret,
    writes: Cell<u32>,
}

impl std::fmt::Debug for ForgejoTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForgejoTools")
            .field("url", &self.config.url)
            .field("login", &self.config.login)
            .field("repositories", &self.config.repositories)
            .field("agent", &self.agent)
            .field("writes", &self.writes.get())
            .finish_non_exhaustive()
    }
}

impl ForgejoTools {
    /// Build the tools for one run, given the non-secret context and the token.
    pub fn new(
        config: RunForgejo,
        workspace: impl Into<PathBuf>,
        agent: impl Into<String>,
        token: Secret,
    ) -> Self {
        Self {
            config,
            workspace: workspace.into(),
            agent: agent.into(),
            token,
            writes: Cell::new(0),
        }
    }

    /// How many writes this run has made.
    pub fn writes(&self) -> u32 {
        self.writes.get()
    }

    /// Count a write the instance accepted, whether or not its reply parsed.
    ///
    /// This is deliberately not "the call returned `Ok`": a 2xx whose body is not the
    /// expected shape still changed the instance, so it must use up one of the
    /// run's writes (B2). A 4xx/5xx refusal never reaches here.
    fn note_accepted(&self, client: &ForgejoClient) {
        if client.last_2xx() {
            self.writes.set(self.writes.get().saturating_add(1));
        }
    }

    /// The login the instance recorded for this agent.
    pub fn login(&self) -> &str {
        &self.config.login
    }

    /// The repositories the run may act on.
    pub fn repositories(&self) -> &[RunForgejoRepository] {
        &self.config.repositories
    }

    /// The client for this call. The token is opened only here, for the header.
    fn client(&self) -> Result<ForgejoClient, String> {
        ForgejoClient::new(
            &self.config.url,
            self.token.clone(),
            forgejo::DEFAULT_TIMEOUT,
        )
        .map_err(|error| self.error_sentence(error))
    }

    /// Check that a name is one of this agent's cloned repositories.
    fn repository(&self, full_name: &str) -> Result<(&RunForgejoRepository, PathBuf), String> {
        let Some(repository) = self.config.repository(full_name) else {
            return Err(self.unknown_repository(full_name));
        };
        let destination = self.workspace.join(&repository.directory);
        if !destination.join(".git").is_dir() {
            return Err(sentence(format!(
                "`{full_name}` is no longer a clone in this agent's workspace; a repository is named `owner/name` and must be cloned there first"
            )));
        }
        Ok((repository, destination))
    }

    fn unknown_repository(&self, full_name: &str) -> String {
        let names = self
            .config
            .repositories
            .iter()
            .map(|repository| format!("`{}`", repository.full_name))
            .collect::<Vec<_>>();
        if names.is_empty() {
            return sentence(
                "no repository is cloned into this agent's workspace, so no Forgejo repository can be used"
                    .to_owned(),
            );
        }
        sentence(format!(
            "`{full_name}` is not cloned in this agent's workspace; only {} can be used",
            names.join(", ")
        ))
    }

    /// Split `owner/name`, refusing anything else with one sentence.
    fn split_repository(&self, full_name: &str) -> Result<(String, String), String> {
        let Some((owner, name)) = full_name.split_once('/') else {
            return Err(sentence(format!(
                "`{full_name}` is not a repository name; write `owner/name`"
            )));
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(sentence(format!(
                "`{full_name}` is not a repository name; write `owner/name`"
            )));
        }
        Ok((owner.to_owned(), name.to_owned()))
    }

    /// One Forgejo failure as one bounded sentence naming the class and status.
    fn error_sentence(&self, error: ForgejoError) -> String {
        let text = match error {
            ForgejoError::MissingScope { rights, .. } if !rights.is_empty() => format!(
                "the agent token for `{}` is missing the Forgejo rights {}; the product makes a new token when the agent is set up again",
                self.agent,
                rights.join(", ")
            ),
            ForgejoError::MissingScope { .. } => format!(
                "the agent token for `{}` is missing a Forgejo right this call needs; the product makes a new token when the agent is set up again",
                self.agent
            ),
            ForgejoError::BadToken { status } => format!(
                "the Forgejo refused the agent's token (bad_token, HTTP {status})"
            ),
            ForgejoError::Forbidden { status, .. } => {
                format!("the Forgejo refused this call (forbidden, HTTP {status})")
            }
            ForgejoError::NotFound { .. } => {
                "the Forgejo has nothing at that repository and number (not_found, HTTP 404)"
                    .to_owned()
            }
            ForgejoError::Server { status, .. } => {
                format!("the Forgejo answered with an error (server_error, HTTP {status})")
            }
            ForgejoError::BadUrl { .. } => {
                "the bundled Forgejo address is not usable (bad_url); the product writes it when it sets the instance up"
                    .to_owned()
            }
            ForgejoError::MissingToken => {
                "the agent token is absent (missing_token); the product records it when the account is ready"
                    .to_owned()
            }
            ForgejoError::Timeout { secs } => {
                format!("the Forgejo did not answer within {secs} s (timeout)")
            }
            ForgejoError::Transport { .. } => {
                "the Forgejo could not be reached (transport)".to_owned()
            }
            ForgejoError::MalformedReply { .. } => {
                "the Forgejo reply is not usable (malformed_reply)".to_owned()
            }
        };
        // The client already redacts its own token from what it keeps; redact once
        // more here so no path can carry the value into a result.
        sentence(self.token.redact(&text))
    }

    /// The number arguments, or one sentence saying what is missing.
    fn number(&self, args: &serde_json::Value) -> Result<u64, String> {
        args.get("number")
            .and_then(serde_json::Value::as_u64)
            .filter(|number| *number > 0)
            .ok_or_else(|| sentence("the arguments need a positive integer `number`".to_owned()))
    }

    /// The repository argument, or one sentence saying what is missing.
    fn repository_arg(&self, args: &serde_json::Value) -> Result<String, String> {
        args.get("repository")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| sentence("the arguments need `repository` as `owner/name`".to_owned()))
    }

    fn title_arg(&self, args: &serde_json::Value) -> Result<String, String> {
        args.get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| sentence("the arguments need a non-empty `title`".to_owned()))
    }

    fn body_arg(&self, args: &serde_json::Value) -> Option<String> {
        args.get("body")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    /// Execute one call and return its text, or one bounded refusal sentence.
    pub fn execute(&mut self, name: &str, arguments: &str) -> Result<ToolReply, String> {
        let args: serde_json::Value = serde_json::from_str(arguments).map_err(|error| {
            sentence(format!(
                "the arguments are not a JSON object ({error}); call `{name}` with the declared schema"
            ))
        })?;
        if !args.is_object() {
            return Err(sentence(format!(
                "the arguments are not a JSON object; call `{name}` with the declared schema"
            )));
        }
        if is_write_tool(name) && self.writes.get() >= WRITE_BOUND {
            return Err(sentence(format!(
                "the per-run Forgejo write bound of {WRITE_BOUND} is reached; this run can still read Forgejo, but no further comment or pull request will be written"
            )));
        }
        match name {
            LIST_ASSIGNED_ISSUES => self.list_assigned_issues(),
            READ_ISSUE_OR_PULL => self.read_issue_or_pull(&args),
            COMMENT => self.comment(&args),
            LIST_MY_PULL_REQUESTS => self.list_my_pull_requests(),
            OPEN_PULL_REQUEST => self.open_pull_request(&args),
            UPDATE_PULL_REQUEST => self.update_pull_request(&args),
            other => Err(sentence(format!(
                "no Forgejo tool named `{other}` is declared; the Forgejo tools are {}",
                TOOL_NAMES.join(", ")
            ))),
        }
    }

    /// The open issues assigned to this agent's account, in its cloned repositories.
    fn list_assigned_issues(&self) -> Result<ToolReply, String> {
        let client = self.client()?;
        let issues = client
            .assigned_issues()
            .map_err(|error| self.error_sentence(error))?;
        let cloned: HashSet<&str> = self
            .config
            .repositories
            .iter()
            .map(|repository| repository.full_name.as_str())
            .collect();
        let mut mine: Vec<&Issue> = issues
            .iter()
            .filter(|issue| {
                issue
                    .repository
                    .as_deref()
                    .is_some_and(|repository| cloned.contains(repository))
            })
            .collect();
        mine.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.number.cmp(&right.number))
        });
        let mut lines: Vec<String> = Vec::new();
        for issue in &mine {
            let repository = issue.repository.as_deref().unwrap_or("?");
            let labels = if issue.labels.is_empty() {
                "no labels".to_owned()
            } else {
                issue.labels.join(", ")
            };
            let updated = issue.updated_at.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "{repository}#{} — {} [{labels}] (updated {updated})",
                issue.number, issue.title
            ));
        }
        if lines.is_empty() {
            return Ok(ToolReply {
                text: format!(
                    "no open issue is assigned to `{}` in the cloned repositories",
                    self.config.login
                ),
                repository: None,
                number: None,
                action: None,
                url: None,
                branch: None,
            });
        }
        let (text, cut) = bounded(&lines.join("\n"), MAX_TOOL_OUTPUT);
        let mut text = text;
        if cut {
            text.push_str("\n[truncated — more assigned issues were not shown]");
        }
        Ok(ToolReply {
            text,
            repository: None,
            number: None,
            action: None,
            url: None,
            branch: None,
        })
    }

    /// One issue or pull request with its comments, bounded and declaring the cut.
    fn read_issue_or_pull(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        let issue = client
            .issue(&owner, &name, number)
            .map_err(|error| self.error_sentence(error))?;
        let comments = client
            .issue_comments(&owner, &name, number, READ_COMMENT_COUNT)
            .map_err(|error| self.error_sentence(error))?;
        let kind = if issue.pull_request_url.is_some() {
            "pull request"
        } else {
            "issue"
        };
        let mut text = format!(
            "{repository}#{number} [{kind}, {}] {}\nopened by {} — updated {}\n",
            issue.state,
            issue.title,
            issue.author.as_deref().unwrap_or("an unnamed account"),
            issue.updated_at.as_deref().unwrap_or("unknown"),
        );
        if let Some(url) = issue
            .pull_request_url
            .as_deref()
            .or(issue.html_url.as_deref())
        {
            text.push_str(&format!("{url}\n"));
        }
        match issue.body.as_deref().map(str::trim) {
            Some(body) if !body.is_empty() => {
                let (body, cut) = bounded(body, READ_BODY_BYTES);
                text.push_str(&format!("\n{body}\n"));
                if cut {
                    text.push_str(&format!(
                        "\n[body truncated after {READ_BODY_BYTES} bytes]\n"
                    ));
                }
            }
            _ => text.push_str("\n(no body)\n"),
        }
        text.push_str(&format!("\ncomments: {}\n", comments.len()));
        for comment in &comments {
            let (body, cut) = bounded(&comment.body, READ_COMMENT_BYTES);
            text.push_str(&format!(
                "\n— {} at {} ({})\n{body}\n",
                comment.author.as_deref().unwrap_or("an unnamed account"),
                comment.created_at.as_deref().unwrap_or("unknown"),
                comment.html_url.as_deref().unwrap_or("no web URL"),
            ));
            if cut {
                text.push_str(&format!(
                    "[comment truncated after {READ_COMMENT_BYTES} bytes]\n"
                ));
            }
        }
        if issue.comments > comments.len() as u64 {
            text.push_str(&format!(
                "\n[{} more comments were not shown]\n",
                issue.comments - comments.len() as u64
            ));
        }
        let (text, cut) = bounded(&text, MAX_TOOL_OUTPUT);
        let mut text = text;
        if cut {
            text.push_str("\n[result truncated at the tool's output bound]");
        }
        Ok(ToolReply {
            text,
            repository: Some(repository),
            number: Some(number),
            action: None,
            url: None,
            branch: None,
        })
    }

    /// One comment on an issue or pull request in a cloned repository.
    fn comment(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let body = self
            .body_arg(args)
            .filter(|body| !body.trim().is_empty())
            .ok_or_else(|| sentence("the arguments need a non-empty `body`".to_owned()))?;
        if body.len() > WRITE_COMMENT_BYTES {
            return Err(sentence(format!(
                "the comment body is {} bytes; the bound is {WRITE_COMMENT_BYTES} bytes — shorten it",
                body.len()
            )));
        }
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        client.begin_write();
        let comment = client.comment_on_issue(&owner, &name, number, &body);
        self.note_accepted(&client);
        let comment = comment.map_err(|error| self.error_sentence(error))?;
        let url = comment
            .html_url
            .clone()
            .unwrap_or_else(|| self.web_url(&repository, number, None));
        Ok(ToolReply {
            text: format!("commented on {repository}#{number} — {url}"),
            repository: Some(repository),
            number: Some(number),
            action: Some("commented"),
            url: Some(url),
            branch: None,
        })
    }

    /// The agent's own open pull requests in its cloned repositories.
    fn list_my_pull_requests(&self) -> Result<ToolReply, String> {
        let client = self.client()?;
        let mut lines: Vec<String> = Vec::new();
        for repository in &self.config.repositories {
            let (owner, name) = match self.split_repository(&repository.full_name) {
                Ok(parts) => parts,
                Err(_) => continue,
            };
            let pulls = client
                .pull_requests(&owner, &name)
                .map_err(|error| self.error_sentence(error))?;
            for pull in pulls {
                if pull.author.as_deref() != Some(self.config.login.as_str()) {
                    continue;
                }
                lines.push(format!(
                    "{}#{} — {} ({} -> {}) {}",
                    repository.full_name,
                    pull.number,
                    pull.title,
                    pull.head,
                    pull.base,
                    pull.html_url.as_deref().unwrap_or("no web URL"),
                ));
            }
        }
        if lines.is_empty() {
            return Ok(ToolReply {
                text: format!(
                    "no open pull request is the agent's own in the cloned repositories (login `{}`)",
                    self.config.login
                ),
                repository: None,
                number: None,
                action: None,
                url: None,
                branch: None,
            });
        }
        let (text, cut) = bounded(&lines.join("\n"), MAX_TOOL_OUTPUT);
        let mut text = text;
        if cut {
            text.push_str("\n[truncated — more of the agent's open pull requests were not shown]");
        }
        Ok(ToolReply {
            text,
            repository: None,
            number: None,
            action: None,
            url: None,
            branch: None,
        })
    }

    /// The clone's branch, its repository, and the instance's default branch.
    fn branch_and_base(
        &self,
        client: &ForgejoClient,
        repository: &str,
        destination: &Path,
    ) -> Result<(String, String), String> {
        let branch = workspace::current_branch(destination).map_err(|error| {
            sentence(format!(
                "the branch of `{repository}` could not be read ({}): {}",
                error.class(),
                error.detail()
            ))
        })?;
        let (owner, name) = self.split_repository(repository)?;
        let base = client
            .repository(&owner, &name)
            .map_err(|error| self.error_sentence(error))?
            .and_then(|repository| repository.default_branch)
            .filter(|branch| !branch.is_empty())
            .or_else(|| workspace::default_branch_of(destination))
            .ok_or_else(|| {
                sentence(format!(
                    "the default branch of `{repository}` could not be read; the pull-request base is unknown"
                ))
            })?;
        Ok((branch, base))
    }

    /// Open a pull request from the clone's current branch into the default branch.
    fn open_pull_request(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let title = self.title_arg(args)?;
        let body = self.body_arg(args).unwrap_or_default();
        let (_, destination) = self.repository(&repository)?;
        let client = self.client()?;
        let (branch, base) = self.branch_and_base(&client, &repository, &destination)?;
        // The person's `P` and this tool share the content gate; a branch `P` refuses
        // is refused here with the same fact.
        pull_gate::content_gate(&destination, &branch, &base)?;
        let (owner, name) = self.split_repository(&repository)?;
        let open = client
            .pull_requests(&owner, &name)
            .map_err(|error| self.error_sentence(error))?;
        if let Some(existing) = open
            .iter()
            .find(|pull| pull_gate::matches_pair(pull, &repository, &branch, &base))
        {
            return Err(self.duplicate_sentence(
                repository.as_str(),
                branch.as_str(),
                base.as_str(),
                existing,
            ));
        }
        client.begin_write();
        let opened = client.open_pull_request(&owner, &name, &title, &body, &branch, &base);
        self.note_accepted(&client);
        match opened {
            Ok(pull) => {
                let url = pull
                    .html_url
                    .clone()
                    .unwrap_or_else(|| self.web_url(&repository, pull.number, None));
                Ok(ToolReply {
                    text: format!("opened {repository}#{} — {url}", pull.number),
                    repository: Some(repository),
                    number: Some(pull.number),
                    action: Some("opened"),
                    url: Some(url),
                    branch: Some(branch.clone()),
                })
            }
            // A race after the lookup: the conflict reply names the pair but not its
            // author. List again and name the request that now exists; never a raw
            // body.
            Err(ForgejoError::Server { status: 409, .. }) => {
                let open = client
                    .pull_requests(&owner, &name)
                    .map_err(|error| self.error_sentence(error))?;
                match open
                    .iter()
                    .find(|pull| pull_gate::matches_pair(pull, &repository, &branch, &base))
                {
                    Some(existing) => Err(self.duplicate_sentence(
                        repository.as_str(),
                        branch.as_str(),
                        base.as_str(),
                        existing,
                    )),
                    None => Err(sentence(format!(
                        "the Forgejo refused the pull request as a duplicate (conflict, HTTP 409) for `{branch}` -> `{base}` in `{repository}`, and the request it names could not be listed"
                    ))),
                }
            }
            Err(error) => Err(self.error_sentence(error)),
        }
    }

    fn duplicate_sentence(
        &self,
        repository: &str,
        branch: &str,
        base: &str,
        existing: &PullRequest,
    ) -> String {
        let authorship =
            PullRequestAuthorship::of(existing.author.as_deref(), Some(self.config.login.as_str()));
        sentence(format!(
            "`{branch}` -> `{base}` in `{repository}` already has an open pull request: {repository}#{} `{}` — {}",
            existing.number,
            existing.title,
            authorship.fact()
        ))
    }

    /// Update the agent's own open pull request for the clone's current branch.
    fn update_pull_request(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let title = match args.get("title") {
            Some(_) => Some(self.title_arg(args)?),
            None => None,
        };
        let body = args
            .get("body")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if title.is_none() && body.is_none() {
            return Err(sentence(
                "the arguments need at least one of `title` or `body` to change".to_owned(),
            ));
        }
        let (_, destination) = self.repository(&repository)?;
        let client = self.client()?;
        let (branch, base) = self.branch_and_base(&client, &repository, &destination)?;
        let (owner, name) = self.split_repository(&repository)?;
        let open = client
            .pull_requests(&owner, &name)
            .map_err(|error| self.error_sentence(error))?;
        let matching: Vec<&PullRequest> = open
            .iter()
            .filter(|pull| pull_gate::matches_pair(pull, &repository, &branch, &base))
            .collect();
        // Only the agent's own request may be updated. The instance should have at
        // most one open request for a pair, but if it reports more, the own one is the
        // one U would act on; a foreign request is named only when none is the
        // agent's.
        let own = matching.iter().find(|pull| {
            PullRequestAuthorship::of(pull.author.as_deref(), Some(self.config.login.as_str()))
                .is_own()
        });
        let Some(found) = own.copied() else {
            if let Some(occupying) = matching.first() {
                let authorship = PullRequestAuthorship::of(
                    occupying.author.as_deref(),
                    Some(self.config.login.as_str()),
                );
                return Err(sentence(format!(
                    "{repository}#{} `{}` is the request for `{branch}` -> `{base}`, but {}; the agent updates only its own open request",
                    occupying.number,
                    occupying.title,
                    authorship.fact()
                )));
            }
            return Err(sentence(format!(
                "no open pull request is for `{branch}` -> `{base}` in `{repository}`, so there is nothing the agent may update"
            )));
        };
        if found.number != number {
            return Err(sentence(format!(
                "the agent's own open pull request for `{branch}` -> `{base}` is {repository}#{}, not #{number}",
                found.number
            )));
        }
        client.begin_write();
        let updated = client.update_pull_request(
            &owner,
            &name,
            found.number,
            title.as_deref(),
            body.as_deref(),
            None,
        );
        self.note_accepted(&client);
        match updated {
            Ok(updated) => {
                let url = updated
                    .html_url
                    .clone()
                    .unwrap_or_else(|| self.web_url(&repository, updated.number, None));
                Ok(ToolReply {
                    text: format!("updated {repository}#{} — {url}", updated.number),
                    repository: Some(repository),
                    number: Some(updated.number),
                    action: Some("updated"),
                    url: Some(url),
                    branch: Some(branch.clone()),
                })
            }
            Err(error) => Err(self.error_sentence(error)),
        }
    }

    /// A web URL built from the configured instance, for a reply that names none.
    fn web_url(&self, repository: &str, number: u64, comment: Option<u64>) -> String {
        let base = self.config.url.trim_end_matches('/');
        match comment {
            Some(comment) => format!("{base}/{repository}/issues/{number}#issuecomment-{comment}"),
            None => format!("{base}/{repository}/issues/{number}"),
        }
    }
}

/// Best-effort facts about a call, read before it runs for the transcript.
pub fn call_facts(_name: &str, arguments: &str) -> CallFacts {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let repository = args
        .get("repository")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let number = args.get("number").and_then(serde_json::Value::as_u64);
    let detail = match args.get("title").and_then(serde_json::Value::as_str) {
        Some(title) => format!("title {:?}", bounded(title, 120).0),
        None => match args.get("body").and_then(serde_json::Value::as_str) {
            Some(body) => format!("body of {} bytes", body.len()),
            None => "no body".to_owned(),
        },
    };
    CallFacts {
        repository,
        number,
        detail,
    }
}

/// One sentence: no newline, trimmed, and bounded so a refusal is never a wall.
pub fn sentence(mut text: String) -> String {
    text = text.replace(['\n', '\r'], " ");
    let text = text.trim().to_owned();
    bounded_text(&text, 600)
}

fn bounded(text: &str, limit: usize) -> (String, bool) {
    let (text, cut) = bounded_with_flag(text, limit);
    (text, cut)
}

fn bounded_with_flag(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn bounded_text(text: &str, limit: usize) -> String {
    // Leave room for the ellipsis inside the limit: a sentence's bound includes the
    // marker that says it was cut.
    let (mut text, cut) = bounded_with_flag(text, limit.saturating_sub('…'.len_utf8()));
    if cut {
        text.push('…');
    }
    text
}

/// Whether a name is a declared Forgejo tool.
pub fn is_forgejo_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// The sentence for a tool that is not declared, used by the run loop.
pub fn undeclared(name: &str, forgejo_declared: bool) -> String {
    if forgejo_declared {
        sentence(format!(
            "no tool named `{name}` is declared; the tools are `shell` and {}",
            TOOL_NAMES.join(", ")
        ))
    } else {
        sentence(format!(
            "no tool named `{name}` is declared; the only tool is `shell`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_has_no_newline_and_is_bounded() {
        let text = sentence(format!("first line\nsecond line {}", "x".repeat(1000)));
        assert!(!text.contains('\n'));
        assert!(text.len() <= 601, "{}", text.len());
    }

    #[test]
    fn truncation_stops_on_a_character_boundary_and_says_so() {
        let text = format!("{}é", "a".repeat(READ_COMMENT_BYTES));
        let (cut, was_cut) = bounded(&text, READ_COMMENT_BYTES);
        assert!(was_cut);
        assert!(cut.len() <= READ_COMMENT_BYTES);
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn only_the_three_writes_count() {
        assert!(is_write_tool(COMMENT));
        assert!(is_write_tool(OPEN_PULL_REQUEST));
        assert!(is_write_tool(UPDATE_PULL_REQUEST));
        assert!(!is_write_tool(LIST_ASSIGNED_ISSUES));
        assert!(!is_write_tool(READ_ISSUE_OR_PULL));
        assert!(!is_write_tool(LIST_MY_PULL_REQUESTS));
    }
}
