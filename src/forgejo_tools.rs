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
//! the authorship rule live in [`crate::pull_gate`] and are shared, not copied. By
//! default a role receives six least-privilege tools (list assigned issues, read one
//! issue/PR with comments, comment, list own open PRs, open a PR from a pushed branch,
//! and update its own open PR). Roles may opt into `triage` (list/create/update issues),
//! `review` (list PRs, read diffs, formal reviews with supervisor self-review block),
//! and `maintain` (merge approved non-own PRs with supervisor self-merge/approval-SHA
//! blocks, and create `team/*` repos). Destructive repo deletion and organisation
//! administration are never exposed.
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

// Tiered opt-in tools (triage / review / maintain).
/// Triage: list all issues in a cloned repository (with state/label filter).
pub const LIST_ISSUES: &str = "forgejo_list_issues";
/// Triage: create a new issue (with optional assignees and labels).
pub const CREATE_ISSUE: &str = "forgejo_create_issue";
/// Triage: update an existing issue (title, body, state, assignees, labels).
pub const UPDATE_ISSUE: &str = "forgejo_update_issue";
/// Review: list open pull requests in a cloned repository.
pub const LIST_PULL_REQUESTS: &str = "forgejo_list_pull_requests";
/// Review: read the unified diff of one pull request (bounded).
pub const READ_PULL_DIFF: &str = "forgejo_read_pull_diff";
/// Review: submit a formal review (`COMMENT`, `APPROVE`, `REQUEST_CHANGES`); self-review hard-blocked.
pub const REVIEW_PULL_REQUEST: &str = "forgejo_review_pull_request";
/// Maintain: merge an approved non-own pull request; self-merge & unapproved merge hard-blocked.
pub const MERGE_PULL_REQUEST: &str = "forgejo_merge_pull_request";
/// Maintain: create a new repository under the `team` organisation (never deletes).
pub const CREATE_REPOSITORY: &str = "forgejo_create_repository";

/// Default least-privilege Forgejo tools, in the order they are declared.
pub const TOOL_NAMES: [&str; 6] = [
    LIST_ASSIGNED_ISSUES,
    READ_ISSUE_OR_PULL,
    COMMENT,
    LIST_MY_PULL_REQUESTS,
    OPEN_PULL_REQUEST,
    UPDATE_PULL_REQUEST,
];

/// Every Forgejo tool across default and opt-in permission tiers.
pub const ALL_TOOL_NAMES: [&str; 14] = [
    LIST_ASSIGNED_ISSUES,
    READ_ISSUE_OR_PULL,
    COMMENT,
    LIST_MY_PULL_REQUESTS,
    OPEN_PULL_REQUEST,
    UPDATE_PULL_REQUEST,
    LIST_ISSUES,
    CREATE_ISSUE,
    UPDATE_ISSUE,
    LIST_PULL_REQUESTS,
    READ_PULL_DIFF,
    REVIEW_PULL_REQUEST,
    MERGE_PULL_REQUEST,
    CREATE_REPOSITORY,
];

/// Tools allowed for one role's permission configuration.
pub fn allowed_tool_names(
    permissions: &crate::engine::settings::RolePermissions,
) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = TOOL_NAMES.to_vec();
    if permissions.triage {
        names.extend([LIST_ISSUES, CREATE_ISSUE, UPDATE_ISSUE]);
    }
    if permissions.review {
        names.extend([LIST_PULL_REQUESTS, READ_PULL_DIFF, REVIEW_PULL_REQUEST]);
    }
    if permissions.maintain {
        names.extend([MERGE_PULL_REQUEST, CREATE_REPOSITORY]);
    }
    names
}

/// Whether a tool writes, and so counts against the role's write bound.
pub fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        COMMENT
            | OPEN_PULL_REQUEST
            | UPDATE_PULL_REQUEST
            | CREATE_ISSUE
            | UPDATE_ISSUE
            | REVIEW_PULL_REQUEST
            | MERGE_PULL_REQUEST
            | CREATE_REPOSITORY
    )
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
    permissions: crate::engine::settings::RolePermissions,
    /// Supervisor-held token used exclusively for `forgejo_create_repository` under
    /// the `team` organisation when `permissions.maintain == true` and the agent's
    /// own token is restricted from org repo creation. Never exposed to the agent
    /// and never used for destructive operations.
    org_creator_token: Option<Secret>,
    writes: Cell<u32>,
    accepted: Cell<bool>,
    viewer_verified: Cell<bool>,
}

impl std::fmt::Debug for ForgejoTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForgejoTools")
            .field("url", &self.config.url)
            .field("login", &self.config.login)
            .field("repositories", &self.config.repositories)
            .field("agent", &self.agent)
            .field("permissions", &self.permissions)
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
        Self::new_with_writes(config, workspace, agent, token, 0)
    }

    /// Build the tools with the writes already made by an earlier MCP server of the
    /// same run, so restarting the bridge cannot reset the write bound.
    pub fn new_with_writes(
        config: RunForgejo,
        workspace: impl Into<PathBuf>,
        agent: impl Into<String>,
        token: Secret,
        writes: u32,
    ) -> Self {
        Self {
            config,
            workspace: workspace.into(),
            agent: agent.into(),
            token,
            permissions: crate::engine::settings::RolePermissions::default(),
            org_creator_token: None,
            writes: Cell::new(writes),
            accepted: Cell::new(false),
            viewer_verified: Cell::new(false),
        }
    }

    /// Attach role-scoped Forgejo permissions to these tools.
    pub fn with_permissions(
        mut self,
        permissions: crate::engine::settings::RolePermissions,
    ) -> Self {
        self.permissions = permissions;
        self
    }

    /// Attach optional supervisor org-repo creation token (only used when
    /// `permissions.maintain == true` for `team/<repo>` creation).
    pub fn with_org_creator_token(mut self, token: Option<Secret>) -> Self {
        self.org_creator_token = token;
        self
    }

    /// Effective per-run write bound for this role.
    pub fn write_bound(&self) -> u32 {
        self.permissions.effective_write_bound()
    }

    /// Role permissions currently active on these tools.
    pub fn permissions(&self) -> &crate::engine::settings::RolePermissions {
        &self.permissions
    }

    /// How many writes this run has made.
    pub fn writes(&self) -> u32 {
        self.writes.get()
    }

    /// Keep the internal count at or above the supervisor's reserved count.
    pub fn set_writes(&self, writes: u32) {
        self.writes.set(writes);
    }

    /// Whether the most recent write request reached a 2xx response, even if its
    /// body could not be read as the expected shape.
    pub fn last_accepted(&self) -> bool {
        self.accepted.get()
    }

    /// Count a write the instance accepted, whether or not its reply parsed.
    ///
    /// This is deliberately not "the call returned `Ok`": a 2xx whose body is not the
    /// expected shape still changed the instance, so it must use up one of the
    /// run's writes (B2). A 4xx/5xx refusal never reaches here.
    fn note_accepted(&self, client: &ForgejoClient) {
        if client.last_2xx() {
            self.accepted.set(true);
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

    /// Verify once that the Forgejo token actually authenticates as `self.config.login`
    /// (case-insensitively, refusing empty logins). Used before review/merge hard blocks
    /// so a forged or stale `forgejo.json` login can never bypass self-review/self-merge.
    fn verify_token_identity(&self, client: &ForgejoClient) -> Result<(), String> {
        let expected = self.config.login.trim();
        if expected.is_empty() {
            return Err(sentence(
                "refused: the role's Forgejo login is empty; cannot verify authorship".to_owned(),
            ));
        }
        if self.viewer_verified.get() {
            return Ok(());
        }
        let actual = client
            .viewer()
            .map_err(|error| self.error_sentence(error))?;
        let actual = actual.login.trim();
        if actual.is_empty() || !actual.eq_ignore_ascii_case(expected) {
            return Err(sentence(format!(
                "refused: Forgejo token authenticates as `{actual}`, which does not match role login `{expected}`"
            )));
        }
        self.viewer_verified.set(true);
        Ok(())
    }

    /// Check that a name is one of this agent's cloned repositories, strictly inside
    /// `self.workspace` without following any symlink outside the workspace.
    fn repository(&self, full_name: &str) -> Result<(&RunForgejoRepository, PathBuf), String> {
        let Some(repository) = self.config.repository(full_name) else {
            return Err(self.unknown_repository(full_name));
        };
        let dir_name = repository.directory.as_str();
        if dir_name.is_empty()
            || dir_name == "."
            || dir_name == ".."
            || dir_name.contains('/')
            || dir_name.contains('\\')
            || dir_name.contains('\0')
        {
            return Err(sentence(format!(
                "`{full_name}` has an invalid clone directory `{dir_name}`"
            )));
        }
        let destination = self.workspace.join(dir_name);
        let Ok(dest_meta) = std::fs::symlink_metadata(&destination) else {
            return Err(sentence(format!(
                "`{full_name}` is no longer a clone in this agent's workspace; a repository is named `owner/name` and must be cloned there first"
            )));
        };
        if dest_meta.file_type().is_symlink() || !dest_meta.file_type().is_dir() {
            return Err(sentence(format!(
                "`{full_name}` clone directory `{dir_name}` must be a real directory inside the workspace, not a symlink"
            )));
        }
        let canon_ws = std::fs::canonicalize(&self.workspace).map_err(|_| {
            sentence("this agent's workspace directory cannot be resolved".to_owned())
        })?;
        let canon_dest = std::fs::canonicalize(&destination).map_err(|_| {
            sentence(format!("`{full_name}` clone directory cannot be resolved"))
        })?;
        if !canon_dest.starts_with(&canon_ws) {
            return Err(sentence(format!(
                "`{full_name}` resolves outside this agent's workspace and is refused"
            )));
        }
        let git_dir = canon_dest.join(".git");
        let git_ok = std::fs::symlink_metadata(&git_dir)
            .map(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
            .unwrap_or(false)
            && std::fs::canonicalize(&git_dir)
                .map(|g| g.starts_with(&canon_dest))
                .unwrap_or(false);
        if !git_ok {
            return Err(sentence(format!(
                "`{full_name}` is no longer a clone in this agent's workspace; a repository is named `owner/name` and must be cloned there first"
            )));
        }
        Ok((repository, canon_dest))
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
        self.accepted.set(false);
        let allowed = allowed_tool_names(&self.permissions);
        if !allowed.contains(&name) {
            return Err(sentence(format!(
                "tool `{name}` is not enabled for this role; allowed Forgejo tools are {}",
                allowed.join(", ")
            )));
        }
        let bound = self.write_bound();
        if is_write_tool(name) && self.writes.get() >= bound {
            return Err(sentence(format!(
                "the per-run Forgejo write bound of {bound} is reached; this run can still read Forgejo, but no further write will be accepted"
            )));
        }
        match name {
            LIST_ASSIGNED_ISSUES => self.list_assigned_issues(),
            READ_ISSUE_OR_PULL => self.read_issue_or_pull(&args),
            COMMENT => self.comment(&args),
            LIST_MY_PULL_REQUESTS => self.list_my_pull_requests(),
            OPEN_PULL_REQUEST => self.open_pull_request(&args),
            UPDATE_PULL_REQUEST => self.update_pull_request(&args),
            LIST_ISSUES => self.list_issues(&args),
            CREATE_ISSUE => self.create_issue(&args),
            UPDATE_ISSUE => self.update_issue(&args),
            LIST_PULL_REQUESTS => self.list_pull_requests(&args),
            READ_PULL_DIFF => self.read_pull_diff(&args),
            REVIEW_PULL_REQUEST => self.review_pull_request(&args),
            MERGE_PULL_REQUEST => self.merge_pull_request(&args),
            CREATE_REPOSITORY => self.create_repository(&args),
            other => Err(sentence(format!(
                "no Forgejo tool named `{other}` is declared; the Forgejo tools are {}",
                allowed.join(", ")
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
                if !crate::pull_gate::PullRequestAuthorship::of(
                    pull.author.as_deref(),
                    Some(self.config.login.as_str()),
                )
                .is_own()
                {
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
            return self.existing_open_reply(
                repository.as_str(),
                branch.as_str(),
                base.as_str(),
                existing,
            );
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
                    Some(existing) => self.existing_open_reply(
                        repository.as_str(),
                        branch.as_str(),
                        base.as_str(),
                        existing,
                    ),
                    None => Err(sentence(format!(
                        "the Forgejo refused the pull request as a duplicate (conflict, HTTP 409) for `{branch}` -> `{base}` in `{repository}`, and the request it names could not be listed"
                    ))),
                }
            }
            Err(error) => Err(self.error_sentence(error)),
        }
    }

    /// The agent's own request for this branch is already open.
    ///
    /// This is the desired end state, not a refused write: the push that preceded
    /// the call is on the request. The reply carries the request's number, URL and
    /// branch so the run's work line can name it. Another account's request stays a
    /// refusal, because this agent may not write to it.
    fn existing_open_reply(
        &self,
        repository: &str,
        branch: &str,
        base: &str,
        existing: &PullRequest,
    ) -> Result<ToolReply, String> {
        let authorship =
            PullRequestAuthorship::of(existing.author.as_deref(), Some(self.config.login.as_str()));
        if !authorship.is_own() {
            return Err(self.duplicate_sentence(repository, branch, base, existing));
        }
        let url = existing
            .html_url
            .clone()
            .unwrap_or_else(|| self.web_url(repository, existing.number, None));
        Ok(ToolReply {
            text: sentence(format!(
                "the pull request for `{branch}` -> `{base}` in `{repository}` is already open: {repository}#{} `{}` — it is the agent's own request; the pushed branch is on it",
                existing.number, existing.title
            )),
            repository: Some(repository.to_owned()),
            number: Some(existing.number),
            action: Some("already_open"),
            url: Some(url),
            branch: Some(branch.to_owned()),
        })
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

    /// Triage: list issues in a cloned repository (with optional `state` and `labels`).
    fn list_issues(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let state = args
            .get("state")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| matches!(*s, "open" | "closed" | "all"))
            .unwrap_or("open");
        let labels = bounded_string_array_arg(args, "labels", 10, 64)?;
        let client = self.client()?;
        let issues = client
            .repo_issues(&owner, &name, Some(state), &labels)
            .map_err(|error| self.error_sentence(error))?;
        let mut lines = Vec::new();
        for issue in issues.iter().filter(|i| i.pull_request_url.is_none()) {
            let assignees = if issue.assignees.is_empty() {
                "unassigned".to_owned()
            } else {
                format!("assigned: {}", issue.assignees.join(", "))
            };
            let lbl = if issue.labels.is_empty() {
                "no labels".to_owned()
            } else {
                issue.labels.join(", ")
            };
            lines.push(format!(
                "{repository}#{} [{}] — {} [{lbl}] ({assignees})",
                issue.number, issue.state, issue.title
            ));
        }
        if lines.is_empty() {
            return Ok(ToolReply {
                text: format!("no {state} issues found in `{repository}`"),
                repository: Some(repository),
                number: None,
                action: None,
                url: None,
                branch: None,
            });
        }
        let (mut text, cut) = bounded(&lines.join("\n"), MAX_TOOL_OUTPUT);
        if cut {
            text.push_str("\n[truncated — more issues were not shown]");
        }
        Ok(ToolReply {
            text,
            repository: Some(repository),
            number: None,
            action: None,
            url: None,
            branch: None,
        })
    }

    /// Triage: create an issue in a cloned repository (with optional assignees and labels).
    fn create_issue(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let title = self.title_arg(args)?;
        let body = self.body_arg(args).unwrap_or_default();
        if body.len() > WRITE_COMMENT_BYTES {
            return Err(sentence(format!(
                "the issue body is {} bytes; the bound is {WRITE_COMMENT_BYTES} bytes — shorten it",
                body.len()
            )));
        }
        let assignees = bounded_string_array_arg(args, "assignees", 10, 64)?;
        let labels = bounded_string_array_arg(args, "labels", 10, 64)?;
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        client.begin_write();
        let created = client.create_issue_full(&owner, &name, &title, &body, &assignees, &labels);
        self.note_accepted(&client);
        let issue = created.map_err(|error| self.error_sentence(error))?;
        let url = issue
            .html_url
            .clone()
            .unwrap_or_else(|| self.web_url(&repository, issue.number, None));
        Ok(ToolReply {
            text: format!("created issue {repository}#{} — {url}", issue.number),
            repository: Some(repository),
            number: Some(issue.number),
            action: Some("created_issue"),
            url: Some(url),
            branch: None,
        })
    }

    /// Triage: update an issue's title, body, state (`open`/`closed`), assignees or labels.
    fn update_issue(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let title = args
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let body = args.get("body").and_then(serde_json::Value::as_str);
        if let Some(b) = body
            && b.len() > WRITE_COMMENT_BYTES
        {
            return Err(sentence(format!(
                "the issue body is {} bytes; the bound is {WRITE_COMMENT_BYTES} bytes",
                b.len()
            )));
        }
        let state = match args.get("state").and_then(serde_json::Value::as_str) {
            Some("open") => Some("open"),
            Some("closed") => Some("closed"),
            Some(other) => {
                return Err(sentence(format!(
                    "`state` must be `open` or `closed` (got `{other}`)"
                )));
            }
            None => None,
        };
        let assignees = match args.get("assignees") {
            Some(_) => Some(bounded_string_array_arg(args, "assignees", 10, 64)?),
            None => None,
        };
        let labels = match args.get("labels") {
            Some(_) => Some(bounded_string_array_arg(args, "labels", 10, 64)?),
            None => None,
        };
        if title.is_none()
            && body.is_none()
            && state.is_none()
            && assignees.is_none()
            && labels.is_none()
        {
            return Err(sentence(
                "provide at least one of `title`, `body`, `state`, `assignees`, or `labels`"
                    .to_owned(),
            ));
        }
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        client.begin_write();
        let updated = client.update_issue(
            &owner,
            &name,
            number,
            title,
            body,
            state,
            assignees.as_deref(),
            labels.as_deref(),
        );
        self.note_accepted(&client);
        let issue = updated.map_err(|error| self.error_sentence(error))?;
        let url = issue
            .html_url
            .clone()
            .unwrap_or_else(|| self.web_url(&repository, issue.number, None));
        Ok(ToolReply {
            text: format!(
                "updated issue {repository}#{} [{}] — {url}",
                issue.number, issue.state
            ),
            repository: Some(repository),
            number: Some(issue.number),
            action: Some("updated_issue"),
            url: Some(url),
            branch: None,
        })
    }

    /// Review: list all open pull requests in a cloned repository.
    fn list_pull_requests(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        let pulls = client
            .pull_requests(&owner, &name)
            .map_err(|error| self.error_sentence(error))?;
        let mut lines = Vec::new();
        for pull in &pulls {
            let author = pull.author.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "{repository}#{} by `{author}` — {} ({} -> {}) {}",
                pull.number,
                pull.title,
                pull.head,
                pull.base,
                pull.html_url.as_deref().unwrap_or("no web URL")
            ));
        }
        if lines.is_empty() {
            return Ok(ToolReply {
                text: format!("no open pull requests in `{repository}`"),
                repository: Some(repository),
                number: None,
                action: None,
                url: None,
                branch: None,
            });
        }
        let (mut text, cut) = bounded(&lines.join("\n"), MAX_TOOL_OUTPUT);
        if cut {
            text.push_str("\n[truncated — more pull requests were not shown]");
        }
        Ok(ToolReply {
            text,
            repository: Some(repository),
            number: None,
            action: None,
            url: None,
            branch: None,
        })
    }

    /// Review: read the unified diff of one pull request (bounded).
    fn read_pull_diff(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        let diff = client
            .pull_diff(&owner, &name, number)
            .map_err(|error| self.error_sentence(error))?;
        let (mut text, cut) = bounded(&diff, MAX_TOOL_OUTPUT);
        if cut {
            text.push_str("\n[diff truncated at the tool's output bound]");
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

    /// Review: submit a formal review (`COMMENT`, `APPROVE`, `REQUEST_CHANGES`).
    /// Supervisor hard-blocks self-review on the role's own pull request.
    fn review_pull_request(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let raw_event = args
            .get("event")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.trim().to_ascii_uppercase())
            .ok_or_else(|| {
                sentence("`event` must be `COMMENT`, `APPROVE`, or `REQUEST_CHANGES`".to_owned())
            })?;
        let api_event = match raw_event.as_str() {
            "COMMENT" => "COMMENT",
            "APPROVE" | "APPROVED" => "APPROVED",
            "REQUEST_CHANGES" => "REQUEST_CHANGES",
            other => {
                return Err(sentence(format!(
                    "`event` must be `COMMENT`, `APPROVE`, or `REQUEST_CHANGES` (got `{other}`)"
                )));
            }
        };
        let body = self.body_arg(args).unwrap_or_default();
        if body.len() > WRITE_COMMENT_BYTES {
            return Err(sentence(format!(
                "the review body is {} bytes; the bound is {WRITE_COMMENT_BYTES} bytes",
                body.len()
            )));
        }
        if api_event != "APPROVED" && body.trim().is_empty() {
            return Err(sentence(
                "reviews with `COMMENT` or `REQUEST_CHANGES` require a non-empty `body`".to_owned(),
            ));
        }
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        self.verify_token_identity(&client)?;
        let Some(pull) = client
            .pull_request(&owner, &name, number)
            .map_err(|error| self.error_sentence(error))?
        else {
            return Err(sentence(format!(
                "pull request {repository}#{number} was not found"
            )));
        };
        if pull.state != "open" {
            return Err(sentence(format!(
                "pull request {repository}#{number} is `{}` and cannot be reviewed",
                pull.state
            )));
        }
        // HARD INVARIANT: fail closed unless PR author is positively proven to be another account.
        let authorship =
            PullRequestAuthorship::of(pull.author.as_deref(), Some(self.config.login.as_str()));
        if !authorship.is_verified_other() {
            return Err(sentence(format!(
                "self-review is refused: {repository}#{number} ({}); another role must review it",
                authorship.fact()
            )));
        }
        client.begin_write();
        let review = client.create_pull_review(&owner, &name, number, api_event, &body);
        self.note_accepted(&client);
        let review = review.map_err(|error| self.error_sentence(error))?;
        let url = review
            .html_url
            .clone()
            .or(pull.html_url.clone())
            .unwrap_or_else(|| self.web_url(&repository, number, None));
        Ok(ToolReply {
            text: format!("reviewed {repository}#{number} ({api_event}) — {url}"),
            repository: Some(repository),
            number: Some(number),
            action: Some("reviewed"),
            url: Some(url),
            branch: Some(pull.head),
        })
    }

    /// Maintain: merge an approved non-own pull request.
    /// Supervisor hard-blocks self-merge and merges without an active non-self `APPROVED` review.
    fn merge_pull_request(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let repository = self.repository_arg(args)?;
        let number = self.number(args)?;
        let method = match args
            .get("method")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("merge") => "merge",
            Some("squash") => "squash",
            Some("rebase") => "rebase",
            Some(other) => {
                return Err(sentence(format!(
                    "`method` must be `merge`, `squash`, or `rebase` (got `{other}`)"
                )));
            }
        };
        let title = args.get("title").and_then(serde_json::Value::as_str);
        let (owner, name) = self.split_repository(&repository)?;
        self.repository(&repository)?;
        let client = self.client()?;
        self.verify_token_identity(&client)?;
        let Some(pull) = client
            .pull_request(&owner, &name, number)
            .map_err(|error| self.error_sentence(error))?
        else {
            return Err(sentence(format!(
                "pull request {repository}#{number} was not found"
            )));
        };
        if pull.state != "open" {
            return Err(sentence(format!(
                "pull request {repository}#{number} is `{}` and cannot be merged",
                pull.state
            )));
        }
        // HARD INVARIANT 1: fail closed unless PR author is positively proven to be another account.
        let authorship =
            PullRequestAuthorship::of(pull.author.as_deref(), Some(self.config.login.as_str()));
        if !authorship.is_verified_other() {
            return Err(sentence(format!(
                "self-merge is refused: {repository}#{number} ({}); another role with maintain permission must merge it",
                authorship.fact()
            )));
        }
        // HARD INVARIANT 2: require at least one active, non-dismissed, non-stale
        // non-author APPROVED review matching the current PR head SHA, and zero
        // unresolved REQUEST_CHANGES reviews across all review pages.
        let reviews = client
            .pull_reviews(&owner, &name, number)
            .map_err(|error| self.error_sentence(error))?;
        if let Err(reason) =
            verify_merge_approval(pull.author.as_deref(), pull.head_sha.as_deref(), &reviews)
        {
            return Err(sentence(format!(
                "merge refused for {repository}#{number}: {reason}"
            )));
        }
        client.begin_write();
        let merged = client.merge_pull_request(
            &owner,
            &name,
            number,
            method,
            title,
            pull.head_sha.as_deref(),
        );
        self.note_accepted(&client);
        merged.map_err(|error| self.error_sentence(error))?;
        let url = pull
            .html_url
            .clone()
            .unwrap_or_else(|| self.web_url(&repository, number, None));
        Ok(ToolReply {
            text: format!("merged {repository}#{number} via {method} — {url}"),
            repository: Some(repository),
            number: Some(number),
            action: Some("merged"),
            url: Some(url),
            branch: Some(pull.head),
        })
    }

    /// Maintain: create a new repository under the `team` organisation (never deletes).
    fn create_repository(&self, args: &serde_json::Value) -> Result<ToolReply, String> {
        let raw_name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| sentence("the arguments need a non-empty repository `name`".to_owned()))?;
        if raw_name.len() > 64
            || raw_name.contains('/')
            || raw_name.contains("..")
            || !raw_name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            || !raw_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(sentence(format!(
                "repository name `{raw_name}` must be 1–64 ASCII letters, digits, `-`, `_`, or `.` starting with a letter or digit"
            )));
        }
        let description = args
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let private = match args.get("private") {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => {
                return Err(sentence(
                    "`private` must be a boolean (`true` or `false`)".to_owned(),
                ));
            }
        };
        let org = crate::bundled_forgejo::ORGANISATION;
        let client = match self.org_creator_token.as_ref() {
            Some(token) => {
                ForgejoClient::new(&self.config.url, token.clone(), forgejo::DEFAULT_TIMEOUT)
                    .map_err(|error| self.error_sentence(error))?
            }
            None => self.client()?,
        };
        client.begin_write();
        let created = client.create_org_repository(org, raw_name, description, private);
        self.note_accepted(&client);
        let repo = created.map_err(|error| self.error_sentence(error))?;
        let url = repo
            .html_url
            .clone()
            .unwrap_or_else(|| format!("{}/{}", self.config.url.trim_end_matches('/'), repo.full_name));
        Ok(ToolReply {
            text: format!("created repository {} — {url}", repo.full_name),
            repository: Some(repo.full_name),
            number: None,
            action: Some("created_repository"),
            url: Some(url),
            branch: repo.default_branch,
        })
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

/// Extract a bounded, deduplicated list of non-empty strings from a JSON array field.
fn bounded_string_array_arg(
    args: &serde_json::Value,
    key: &str,
    max_items: usize,
    max_len: usize,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let Some(raw) = args.get(key) else {
        return Ok(out);
    };
    let Some(arr) = raw.as_array() else {
        return Err(sentence(format!("`{key}` must be an array of strings")));
    };
    if arr.len() > max_items {
        return Err(sentence(format!(
            "`{key}` accepts at most {max_items} items per call (got {})",
            arr.len()
        )));
    }
    for item in arr {
        let Some(s) = item.as_str() else {
            return Err(sentence(format!("`{key}` must contain only strings")));
        };
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > max_len {
            return Err(sentence(format!(
                "each item in `{key}` must be at most {max_len} bytes"
            )));
        }
        if !out.iter().any(|existing: &String| existing.eq_ignore_ascii_case(trimmed)) {
            out.push(trimmed.to_owned());
        }
    }
    Ok(out)
}

/// Evaluate whether a pull request has an active, non-dismissed, non-stale
/// non-author `APPROVED` review matching the current PR head SHA and zero
/// unresolved non-author `REQUEST_CHANGES` reviews.
pub(crate) fn verify_merge_approval(
    pr_author: Option<&str>,
    pr_head_sha: Option<&str>,
    reviews: &[crate::forgejo::PullReview],
) -> Result<(), &'static str> {
    let Some(pr_author) = pr_author.map(str::trim).filter(|a| !a.is_empty()) else {
        return Err("pull request author is missing; refusing merge");
    };
    let Some(expected_head) = pr_head_sha.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("pull request head commit SHA is missing; refusing unpinned merge");
    };

    // Sort deterministically by (submitted_at, id) so response ordering cannot
    // hide a newer REQUEST_CHANGES or inflate an older APPROVED review.
    let mut ordered: Vec<&crate::forgejo::PullReview> = reviews.iter().collect();
    ordered.sort_by(|a, b| {
        a.submitted_at
            .as_deref()
            .unwrap_or("")
            .cmp(b.submitted_at.as_deref().unwrap_or(""))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut latest_by_reviewer: std::collections::BTreeMap<String, &'static str> =
        std::collections::BTreeMap::new();
    for review in ordered {
        let Some(reviewer) = review
            .author
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        else {
            continue;
        };
        if pr_author.eq_ignore_ascii_case(reviewer) {
            continue;
        }
        let key = reviewer.to_ascii_lowercase();
        let state = review.state.trim().to_ascii_uppercase();
        if review.dismissed || state == "DISMISSED" {
            latest_by_reviewer.insert(key, "DISMISSED");
            continue;
        }
        match state.as_str() {
            "REQUEST_CHANGES" => {
                latest_by_reviewer.insert(key, "REQUEST_CHANGES");
            }
            "APPROVED" => {
                let sha_matches = review
                    .commit_id
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|cid| cid.eq_ignore_ascii_case(expected_head));
                if !review.stale && sha_matches {
                    latest_by_reviewer.insert(key, "APPROVED");
                } else {
                    // A stale or older-commit approval does not satisfy current head
                    // approval; preserve an earlier REQUEST_CHANGES if still unresolved.
                    if latest_by_reviewer.get(&key).copied() != Some("REQUEST_CHANGES") {
                        latest_by_reviewer.insert(key, "STALE_APPROVAL");
                    }
                }
            }
            _ => {}
        }
    }
    if latest_by_reviewer.values().any(|s| *s == "REQUEST_CHANGES") {
        return Err("a reviewer has an unresolved REQUEST_CHANGES review");
    }
    if !latest_by_reviewer.values().any(|s| *s == "APPROVED") {
        return Err(
            "at least one current, non-dismissed, non-author APPROVED review matching the PR head commit is required before merging",
        );
    }
    Ok(())
}

/// Whether a name is a declared Forgejo tool.
pub fn is_forgejo_tool(name: &str) -> bool {
    ALL_TOOL_NAMES.contains(&name)
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
    fn a_restarted_bridge_starts_from_the_persisted_write_count() {
        let tools = ForgejoTools::new_with_writes(
            crate::run::RunForgejo {
                url: "http://127.0.0.1:1".to_owned(),
                login: "gewu-agent-1".to_owned(),
                repositories: Vec::new(),
            },
            "/tmp/workspace",
            "agent-1",
            Secret::new("dummy"),
            7,
        );
        assert_eq!(tools.writes(), 7);
    }

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
    fn write_and_read_tool_classification_matches_all_tiers() {
        for write_tool in [
            COMMENT,
            OPEN_PULL_REQUEST,
            UPDATE_PULL_REQUEST,
            CREATE_ISSUE,
            UPDATE_ISSUE,
            REVIEW_PULL_REQUEST,
            MERGE_PULL_REQUEST,
            CREATE_REPOSITORY,
        ] {
            assert!(is_write_tool(write_tool), "{write_tool} must be a write tool");
        }
        for read_tool in [
            LIST_ASSIGNED_ISSUES,
            READ_ISSUE_OR_PULL,
            LIST_MY_PULL_REQUESTS,
            LIST_ISSUES,
            LIST_PULL_REQUESTS,
            READ_PULL_DIFF,
        ] {
            assert!(!is_write_tool(read_tool), "{read_tool} must be a read tool");
        }
    }

    #[test]
    fn merge_approval_rejects_stale_dismissed_wrong_sha_missing_author_and_reversed_order() {
        use crate::forgejo::PullReview;
        let rev = |id, author: Option<&str>, state: &str, commit: Option<&str>, dismissed, stale, ts: &str| {
            PullReview {
                id,
                state: state.to_owned(),
                body: String::new(),
                author: author.map(str::to_owned),
                commit_id: commit.map(str::to_owned),
                dismissed,
                stale,
                submitted_at: Some(ts.to_owned()),
                html_url: None,
            }
        };
        // Missing PR author fails closed.
        assert!(
            verify_merge_approval(
                None,
                Some("sha-head"),
                &[rev(1, Some("gewu-rev"), "APPROVED", Some("sha-head"), false, false, "2026-11-27T01:00:00Z")]
            )
            .is_err()
        );
        // Missing PR head SHA fails closed (refuses unpinned merge).
        assert!(
            verify_merge_approval(
                Some("gewu-coder"),
                None,
                &[rev(1, Some("gewu-rev"), "APPROVED", Some("sha-head"), false, false, "2026-11-27T01:00:00Z")]
            )
            .is_err()
        );
        // Stale / older-commit approval after force-push is rejected.
        assert!(
            verify_merge_approval(
                Some("gewu-coder"),
                Some("sha-v2"),
                &[rev(1, Some("gewu-rev"), "APPROVED", Some("sha-v1"), false, false, "2026-11-27T01:00:00Z")]
            )
            .is_err()
        );
        // Dismissed approval is rejected.
        assert!(
            verify_merge_approval(
                Some("gewu-coder"),
                Some("sha-v1"),
                &[rev(1, Some("gewu-rev"), "APPROVED", Some("sha-v1"), true, false, "2026-11-27T01:00:00Z")]
            )
            .is_err()
        );
        // Reversed response order: newer REQUEST_CHANGES beats older APPROVED.
        assert!(
            verify_merge_approval(
                Some("gewu-coder"),
                Some("sha-v1"),
                &[
                    rev(2, Some("gewu-rev"), "REQUEST_CHANGES", Some("sha-v1"), false, false, "2026-11-27T02:00:00Z"),
                    rev(1, Some("gewu-rev"), "APPROVED", Some("sha-v1"), false, false, "2026-11-27T01:00:00Z"),
                ]
            )
            .is_err()
        );
        // Current non-dismissed, non-stale non-author approval on current head SHA passes.
        assert!(
            verify_merge_approval(
                Some("gewu-coder"),
                Some("sha-v2"),
                &[rev(3, Some("gewu-rev"), "APPROVED", Some("sha-v2"), false, false, "2026-11-27T03:00:00Z")]
            )
            .is_ok()
        );
    }

    #[test]
    fn symlinked_clone_directory_outside_workspace_is_refused() {
        let root = crate::test_support::scratch_root("tools-symlink-clone");
        let ws = root.join("ws");
        let sibling = root.join("sibling-repo");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(sibling.join(".git")).unwrap();
        std::os::unix::fs::symlink(&sibling, ws.join("workspace")).unwrap();
        let tools = ForgejoTools::new(
            crate::run::RunForgejo {
                url: "http://127.0.0.1:1".to_owned(),
                login: "gewu-coder".to_owned(),
                repositories: vec![crate::run::RunForgejoRepository {
                    full_name: "team/workspace".to_owned(),
                    directory: "workspace".to_owned(),
                }],
            },
            &ws,
            "coder",
            Secret::new("dummy"),
        );
        let err = tools.repository("team/workspace").expect_err("symlink refused");
        assert!(err.contains("symlink"), "{err}");
    }
}
