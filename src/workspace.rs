//! Git work in an agent's workspace, under the agent's own identity.
//!
//! The tool needs no `glab` and no other Git host client. It runs `git` — the user's
//! own `git`, in the agent's workspace — and configures the two things a commit needs:
//!
//! - **who authored it**: the repository's local `user.name` is the agent's Forgejo
//!   login and `user.email` is the email recorded for that account, so a commit made
//!   in the workspace is authored by the agent rather than by the person running the
//!   tool;
//! - **how it authenticates**: git is started with `GIT_ASKPASS` pointing back at
//!   this same program, which hands git the agent's token **from the credentials
//!   file** over a pipe. The token is never a URL, never a command-line argument and
//!   never a line in a log. The login travels in the clone URL; the secret does not.
//!
//! [`askpass_main`] is that pipe. It reads the credentials path and the agent's name
//! from the environment (paths and a name, never a secret) and writes the token to
//! standard output, which is exactly where git reads it from. On any problem it
//! writes nothing and fails, so a wrong setup is an authentication failure rather
//! than a leaked secret.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::credentials;

/// The environment variable naming the credentials file, for the askpass mode.
pub const ASKPASS_CREDENTIALS: &str = "GEWU_GIT_CREDENTIALS";
/// The environment variable naming the agent, for the askpass mode.
pub const ASKPASS_AGENT: &str = "GEWU_GIT_AGENT";
/// The environment variable naming the login, for the askpass mode (not a secret).
pub const ASKPASS_LOGIN: &str = "GEWU_GIT_LOGIN";

/// The parts git needs to authenticate as one agent.
///
/// A `GitCredential` holds no secret: it names the file the secret is read from, the
/// agent to read it for, and the login to use.
#[derive(Clone)]
pub struct GitCredential<'a> {
    /// This program, which git calls back for the token.
    pub askpass: &'a Path,
    /// The credentials file the token is read from.
    pub credentials: &'a Path,
    /// The agent whose token is used.
    pub agent: &'a str,
    /// The Forgejo login, which git may be asked for.
    pub login: &'a str,
}

/// The identity a commit in one workspace is authored by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIdentity {
    /// The Forgejo account's login.
    pub login: String,
    /// The email recorded for that account.
    pub email: String,
}

impl GitIdentity {
    /// The identity, as one line.
    pub fn line(&self) -> String {
        crate::words::identity_pair(&self.login, &self.email)
    }
}

/// Why a git operation did not finish.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// There is no `git` to run.
    #[error(
        "`git` could not be run: install git, or put it on PATH, because the tool works with the user's own git"
    )]
    GitMissing {
        /// The reason from the operating system.
        source: std::io::Error,
    },
    /// Git ran and refused.
    #[error("`{command}` failed: {message}")]
    Git {
        /// The command, without any secret in it.
        command: String,
        /// What git said, bounded.
        message: String,
    },
    /// The directory is not a git repository.
    #[error("{path} is not a git repository, so nothing can be opened there")]
    NotARepository {
        /// The directory that was asked for.
        path: PathBuf,
    },
    /// The destination is in the way.
    #[error("{path} is not empty and is not a git repository, so a clone would not be safe there")]
    DestinationInTheWay {
        /// The destination that was refused.
        path: PathBuf,
    },
    /// The credentials file could not be read.
    #[error(transparent)]
    Credentials(#[from] credentials::CredentialsError),
    /// The workspace could not be prepared.
    #[error("the workspace {path} cannot be prepared: {source}")]
    Workspace {
        /// The directory that failed.
        path: PathBuf,
        /// The reason from the filesystem.
        source: std::io::Error,
    },
}

impl WorkspaceError {
    /// The underlying reason, led with, for a status line.
    ///
    /// Git's own message is what a person needs; the command it ran can be long and
    /// pushes the reason past the edge of a narrow terminal.
    pub fn detail(&self) -> String {
        match self {
            WorkspaceError::Git { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }

    /// The class of failure, as one word a screen can hold.
    pub fn class(&self) -> &'static str {
        match self {
            WorkspaceError::GitMissing { .. } => "git_missing",
            WorkspaceError::Git { .. } => "git",
            WorkspaceError::NotARepository { .. } => "not_a_repository",
            WorkspaceError::DestinationInTheWay { .. } => "destination_in_the_way",
            WorkspaceError::Credentials(_) => "credentials",
            WorkspaceError::Workspace { .. } => "workspace",
        }
    }
}

/// The clone URL for a repository, with the agent's login in it.
///
/// Only the login goes in the URL. The password is what git asks for, and git is
/// answered by [`askpass_main`] from the credentials file over a pipe.
pub fn clone_url(base: &str, full_name: &str, login: &str) -> String {
    let base = base.trim_end_matches('/');
    match base.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{login}@{rest}/{full_name}.git"),
        None => format!("{base}/{full_name}.git"),
    }
}

/// The directory one repository is cloned into inside an agent's workspace.
pub fn clone_destination(workspace: &Path, full_name: &str) -> PathBuf {
    let name = full_name.rsplit('/').next().unwrap_or(full_name);
    workspace.join(name)
}

/// What `prepare` did at the destination.
///
/// A clone into an empty destination and the reuse of a directory that already holds
/// a git repository are two different acts, and the status must say which one
/// happened (F2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepared {
    /// The repository was cloned into the destination.
    Cloned,
    /// A git repository was already at the destination; it was kept as it was, and
    /// nothing was cloned or fetched. Only its local git identity was set.
    AlreadyPresent,
}

/// Clone a repository into the workspace, or open the clone that is already there.
///
/// This is the "clone or open" of the design: if the destination already holds a git
/// repository it is opened and its identity is (re)applied; otherwise it is cloned.
/// Either way the agent's git identity is configured before the call returns, and the
/// returned [`Prepared`] says which act happened so no surface calls a reuse a clone
/// (F2).
pub fn prepare(
    base: &str,
    full_name: &str,
    workspace: &Path,
    destination: &Path,
    identity: &GitIdentity,
    credential: Option<&GitCredential<'_>>,
) -> Result<(PathBuf, Prepared), WorkspaceError> {
    if destination.join(".git").is_dir() {
        // The directory is a repository already; keep it. Nothing is cloned or
        // fetched, and the caller reports what was found rather than a clone.
        set_identity(destination, identity)?;
        return Ok((destination.to_path_buf(), Prepared::AlreadyPresent));
    }
    if destination.exists() {
        let empty = std::fs::read_dir(destination)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(WorkspaceError::DestinationInTheWay {
                path: destination.to_path_buf(),
            });
        }
    }
    std::fs::create_dir_all(workspace).map_err(|source| WorkspaceError::Workspace {
        path: workspace.to_path_buf(),
        source,
    })?;
    // F-1: the workspace is one of the product's directories.
    crate::mode::set_mode(workspace, crate::mode::PRIVATE_DIR_MODE).map_err(|source| {
        WorkspaceError::Workspace {
            path: workspace.to_path_buf(),
            source,
        }
    })?;

    let login = credential.map(|credential| credential.login).unwrap_or("");
    let url = if login.is_empty() {
        format!("{}/{full_name}.git", base.trim_end_matches('/'))
    } else {
        clone_url(base, full_name, login)
    };
    let destination_text = destination.display().to_string();
    run_git(&["clone", &url, &destination_text], workspace, credential)?;
    set_identity(destination, identity)?;
    Ok((destination.to_path_buf(), Prepared::Cloned))
}

/// Set the repository's local git identity, so a commit is authored by the agent.
pub fn set_identity(repository: &Path, identity: &GitIdentity) -> Result<(), WorkspaceError> {
    if !repository.join(".git").exists() {
        return Err(WorkspaceError::NotARepository {
            path: repository.to_path_buf(),
        });
    }
    run_git(&["config", "user.name", &identity.login], repository, None)?;
    run_git(&["config", "user.email", &identity.email], repository, None)?;
    Ok(())
}

/// What a workspace's own repository is, as far as the product can read it.
///
/// A workspace with history is otherwise only a path on the Agent screen, so a
/// person about to run an agent cannot tell what branch or commit it is on, whether
/// it is dirty, or what local git identity the repository is configured with (the
/// identity the next commit would carry — W-2, w46 F4). Every field here is read
/// from the repository itself, or is `None` when git cannot answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    /// The checked-out branch, or the short commit when HEAD is detached.
    pub branch: String,
    /// The short object name of HEAD, or empty in a repository with no commits.
    pub commit: String,
    /// When HEAD was committed, in unix seconds; `0` with no commits.
    pub committed_at: u64,
    /// How many paths `git status` reports changed.
    pub changed: usize,
    /// The repository's local `user.name <user.email>`, when one is set.
    pub local_identity: Option<String>,
}

/// Read the repository in `workspace`, when there is one.
///
/// `Ok(None)` is a directory that holds no repository — the product says so plainly
/// rather than guessing. `Err(GitMissing)` is a machine with no git, which is also
/// said plainly (W-2). A repository with no commits is a state, not an error: a fresh
/// `git init` has no resolvable `HEAD`, and asking for one used to surface git's own
/// `fatal: ambiguous argument 'HEAD'` on the Agent screen (T-5). The branch is read
/// from the symbolic ref in that case, and the empty history is reported as a state.
pub fn inspect(workspace: &Path) -> Result<Option<WorkspaceState>, WorkspaceError> {
    if !workspace.join(".git").exists() {
        return Ok(None);
    }
    let branch = match run_git(&["rev-parse", "--abbrev-ref", "HEAD"], workspace, None) {
        Ok(text) => text.trim().to_owned(),
        // No commit yet: `rev-parse` cannot resolve `HEAD`, but the checked-out branch
        // still has a name. Neither failure is fatal; only a repository git cannot
        // read at all reaches the `?` on the status read below.
        Err(_) => run_git(&["symbolic-ref", "--short", "HEAD"], workspace, None)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default(),
    };
    // A repository with no commit has no HEAD to log; that is an empty history, not
    // a failure to read one.
    let head = run_git(&["log", "-1", "--format=%h%n%ct"], workspace, None).unwrap_or_default();
    let mut head_lines = head.lines();
    let commit = head_lines.next().unwrap_or("").trim().to_owned();
    let committed_at = head_lines.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let status = run_git(&["status", "--porcelain"], workspace, None)?;
    let changed = status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let name = run_git(&["config", "user.name"], workspace, None)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let email = run_git(&["config", "user.email"], workspace, None)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let local_identity = match (name, email) {
        (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
        (Some(name), None) => Some(name),
        _ => None,
    };
    Ok(Some(WorkspaceState {
        branch,
        commit,
        committed_at,
        changed,
        local_identity,
    }))
}

/// `owner/name` from a repository's `origin` URL, when the URL names one.
///
/// The clone the product makes puts the agent's login in an `http(s)` URL; a person's
/// own clone may use an SSH URL. Only the last two path segments are kept, so a
/// Forgejo served under a path still names the repository the same way. A URL that
/// does not hold two segments is not guessed.
pub fn remote_full_name(repository: &Path) -> Option<String> {
    let url = remote_url(repository)?;
    full_name_from_remote(&url)
}

/// The repository's `origin` URL, when git can read one.
pub fn remote_url(repository: &Path) -> Option<String> {
    let url = run_git(&["config", "--get", "remote.origin.url"], repository, None).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_owned())
}

/// The host and `owner/name` one clone URL names, in one read.
///
/// The clone URL is the only fact the product has about which repository a directory
/// holds; parsing both parts together keeps a screen from running git twice for one
/// answer.
pub fn remote_parts(url: &str) -> Option<(String, String)> {
    let full_name = full_name_from_remote(url)?;
    let host = if let Some((_, rest)) = url.split_once("://") {
        let host = rest.split('/').next()?;
        host.rsplit('@').next().unwrap_or(host).to_owned()
    } else {
        let (host, _) = url.split_once(':')?;
        host.rsplit('@').next().unwrap_or(host).to_owned()
    };
    Some((host, full_name))
}

/// The `owner/name` a clone URL names, when it names one.
pub fn full_name_from_remote(url: &str) -> Option<String> {
    let path = if let Some((_, rest)) = url.split_once("://") {
        rest.split_once('/').map(|(_, path)| path)?
    } else if let Some((_, path)) = url.split_once(':') {
        path
    } else {
        return None;
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplit('/');
    let name = parts.next()?;
    let owner = parts.next()?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// The host (and port, when one is written) of a repository's `origin` URL.
///
/// Used to keep a clone of another instance out of this instance's tool list: a
/// repository is only "in the configured Forgejo" when the clone came from it.
pub fn remote_host(repository: &Path) -> Option<String> {
    let url = remote_url(repository)?;
    remote_parts(&url).map(|(host, _)| host)
}

/// The clone's default branch, from the remote-tracking `origin/HEAD` it carries.
///
/// The product does not fetch to answer this: a clone without `origin/HEAD` has no
/// recorded default branch, and the caller says it does not know rather than guessing
/// `main`.
pub fn default_branch_of(repository: &Path) -> Option<String> {
    let head = run_git(
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        repository,
        None,
    )
    .ok()?;
    let head = head.trim();
    head.strip_prefix("origin/")
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
}

/// The branch a workspace's repository is on.
///
/// A repository with no commit has no resolvable `HEAD` but still has a checked-out
/// branch; that state is a branch name, not a git `fatal:` sentence (T-5).
pub fn current_branch(repository: &Path) -> Result<String, WorkspaceError> {
    match run_git(&["rev-parse", "--abbrev-ref", "HEAD"], repository, None) {
        Ok(text) => Ok(text.trim().to_owned()),
        Err(_) => {
            let text = run_git(&["symbolic-ref", "--short", "HEAD"], repository, None)?;
            Ok(text.trim().to_owned())
        }
    }
}

/// How many commits `branch` has that `base` does not.
///
/// This is the local half of the pull-request gate: a branch that is level with its
/// base has nothing to open. The real base is preferred: when the clone carries the
/// remote-tracking `refs/remotes/origin/<base>`, the count is measured against it, so a
/// fetch that has not updated the local base cannot hide an empty pull request. The
/// local base is used only when the clone has no remote-tracking base. Nothing is
/// fetched.
pub fn commits_ahead(repository: &Path, branch: &str, base: &str) -> Result<usize, WorkspaceError> {
    commits_ahead_against(repository, branch, base).map(|(count, _)| count)
}

/// How many commits `branch` has that `base` does not, and the ref measured.
///
/// The second value is the ref the count was measured against — `origin/<base>` for the
/// remote-tracking base, `<base>` for the local fallback — so a refusal can name the
/// base it actually consulted.
pub fn commits_ahead_against(
    repository: &Path,
    branch: &str,
    base: &str,
) -> Result<(usize, String), WorkspaceError> {
    let remote_ref = format!("refs/remotes/origin/{base}");
    if run_git(
        &["rev-parse", "--verify", "--quiet", &remote_ref],
        repository,
        None,
    )
    .is_ok()
    {
        let remote = format!("{remote_ref}..{branch}");
        let text = run_git(&["rev-list", "--count", &remote], repository, None)?;
        let count = parse_commit_count(&remote, &text)?;
        return Ok((count, format!("origin/{base}")));
    }
    let local = format!("{base}..{branch}");
    let text = run_git(&["rev-list", "--count", &local], repository, None)?;
    let count = parse_commit_count(&local, &text)?;
    Ok((count, base.to_owned()))
}

/// The subjects of the commits `branch` has beyond `base`, newest first.
///
/// This is the neutral text a proposal falls back to when no hand-written file and no
/// run's push witness name the branch. The same measured base as the gate is used, and
/// the list is bounded so a long branch cannot turn a preview into a log.
pub fn commit_subjects_ahead(
    repository: &Path,
    branch: &str,
    base: &str,
    limit: usize,
) -> Result<(Vec<String>, String), WorkspaceError> {
    let remote_ref = format!("refs/remotes/origin/{base}");
    let measured = if run_git(
        &["rev-parse", "--verify", "--quiet", &remote_ref],
        repository,
        None,
    )
    .is_ok()
    {
        format!("origin/{base}")
    } else {
        base.to_owned()
    };
    let range = format!("{measured}..{branch}");
    let text = run_git(
        &[
            "log",
            "--format=%s",
            "-n",
            &limit.max(1).to_string(),
            &range,
        ],
        repository,
        None,
    )?;
    let subjects = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Ok((subjects, measured))
}

fn parse_commit_count(spec: &str, text: &str) -> Result<usize, WorkspaceError> {
    text.trim()
        .parse::<usize>()
        .map_err(|_| WorkspaceError::Git {
            command: format!("git rev-list --count {spec}"),
            message: format!("the count was not a number: {text:?}"),
        })
}

/// Whether the clone carries the remote-tracking ref `refs/remotes/origin/<branch>`.
///
/// This is the selected clone's own evidence that its branch is on the remote, and the
/// pull-request gate's third fact. The ref is named exactly, not matched by suffix:
/// another remote's tracking ref is not this clone's `origin`. This costs no network
/// call while the Forgejo screen is being drawn.
pub fn branch_on_remote(repository: &Path, branch: &str) -> bool {
    let refname = format!("refs/remotes/origin/{branch}");
    let Ok(text) = run_git(
        &["for-each-ref", "--format=%(refname)", &refname],
        repository,
        None,
    ) else {
        return false;
    };
    text.lines().any(|name| name.trim() == refname)
}

/// One repository cloned into a directory below a workspace root.
///
/// The product's own clone puts a repository at `workspace/<repo>`, so the workspace
/// root is often not itself a repository while a clone stands one level down (R-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonedRepository {
    /// The directory name inside the workspace.
    pub name: String,
    /// What could be read of the repository, when it could be read.
    pub state: Option<WorkspaceState>,
}

/// The git repositories directly below `workspace`, in name order.
///
/// Used when the workspace root is not a git repository: rather than reading the
/// clone the product itself made as “not a git repository”, the screen names it.
pub fn cloned_repositories(workspace: &Path) -> Vec<ClonedRepository> {
    let Ok(entries) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut found: Vec<ClonedRepository> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        if !entry.path().join(".git").exists() {
            continue;
        }
        found.push(ClonedRepository {
            name,
            state: inspect(&entry.path()).ok().flatten(),
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Fetch every remote ref into an existing clone, without touching local work.
///
/// The session loop calls this before a session when the repository is already in the
/// agent's workspace. It updates the remote-tracking refs only: no branch is reset,
/// checked out or discarded, so uncommitted work and local commits stay as they are.
pub fn fetch(repository: &Path, credential: &GitCredential<'_>) -> Result<(), WorkspaceError> {
    run_git(&["fetch", "--all", "--prune"], repository, Some(credential))?;
    Ok(())
}

/// Push the current branch to `origin`, as the agent.
pub fn push(repository: &Path, credential: &GitCredential<'_>) -> Result<(), WorkspaceError> {
    run_git(
        &["push", "--set-upstream", "origin", "HEAD"],
        repository,
        Some(credential),
    )?;
    Ok(())
}

/// What a run changed in its workspace, and whether it changed Git state.
///
/// The snapshot hashes every worktree file and records its mode and type, records
/// directories (so an empty directory is visible) and symlink targets, and keeps the
/// Git files that prove commits, refs and tags. `index` and `objects` are handled by
/// the working-tree status comparison, so a refresh of the index does not look like
/// work while a staged change does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChanges {
    /// Worktree paths that differ from the before-picture, sorted.
    #[serde(default)]
    pub files: Vec<String>,
    /// A local commit was made (a local branch ref or `HEAD` changed).
    #[serde(default)]
    pub committed: bool,
    /// Other Git state changed: staged changes, tags, remote-tracking refs, config.
    #[serde(default)]
    pub git_state: bool,
}

impl WorkspaceChanges {
    /// Whether the run left anything local.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && !self.committed && !self.git_state
    }

    /// The work row this change is shown as, or `None` when nothing changed.
    ///
    /// The row names the paths rather than saying only "workspace changed": a person
    /// needs to know which local work the warning is about.
    pub fn work_line(&self) -> Option<String> {
        if !self.files.is_empty() {
            let shown: Vec<&str> = self.files.iter().take(3).map(String::as_str).collect();
            let mut line = format!("local change {}", shown.join(", "));
            let rest = self.files.len().saturating_sub(shown.len());
            if rest > 0 {
                line.push_str(&format!(" +{rest}"));
            }
            return Some(line);
        }
        if self.committed {
            return Some("local commit".to_owned());
        }
        self.git_state.then(|| "local git change".to_owned())
    }

    /// The short reason sentence for a local-only run, naming the work.
    pub fn reason(&self) -> Option<String> {
        if !self.files.is_empty() {
            let shown: Vec<&str> = self.files.iter().take(3).map(String::as_str).collect();
            let mut named = shown.join(", ");
            let rest = self.files.len().saturating_sub(shown.len());
            if rest > 0 {
                named.push_str(&format!(" and {rest} more"));
            }
            return Some(format!(
                "workspace changed: {named}; the change is local and was not pushed"
            ));
        }
        if self.committed {
            return Some(
                "workspace changed: a local commit; the commit is local and was not pushed"
                    .to_owned(),
            );
        }
        self.git_state.then(|| {
            "workspace changed: local Git state; the change is local and was not pushed".to_owned()
        })
    }
}

/// One file, directory, link or special entry in the before/after picture.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File {
        mode: u32,
        modified: u128,
        digest: [u8; 32],
    },
    Dir {
        mode: u32,
    },
    Symlink {
        target: PathBuf,
    },
    Other {
        mode: u32,
    },
}

/// The before-picture of an agent's workspace.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSnapshot {
    entries: BTreeMap<PathBuf, Entry>,
    /// The worktree status of every repository the run can touch, so a staged change
    /// (which lives in the excluded index) is still a local fact.
    git_status: BTreeMap<PathBuf, String>,
}

/// Take the before-picture of an agent's workspace.
///
/// Worktree files are compared by content, mode, type and modification time.
/// Directories are included so an empty directory is visible, and a symlink records
/// its target. In `.git`, the files that prove commits, refs, tags and other state
/// are included; `objects` and `index` are skipped because they are large and the
/// index's meaningful changes appear in the worktree status comparison.
pub fn snapshot_workspace(root: &Path) -> WorkspaceSnapshot {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if git_skipped(&relative) {
                continue;
            }
            let kind = meta.file_type();
            let mode = meta.permissions().mode() & 0o7777;
            if kind.is_dir() {
                out.insert(relative.clone(), Entry::Dir { mode });
                walk(root, &path, out);
            } else if kind.is_file() {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as u64)
                    .unwrap_or(0) as u128;
                out.insert(
                    relative,
                    Entry::File {
                        mode,
                        modified,
                        digest: digest_file(&path),
                    },
                );
            } else if kind.is_symlink() {
                if let Ok(target) = std::fs::read_link(&path) {
                    out.insert(relative, Entry::Symlink { target });
                }
            } else {
                out.insert(relative, Entry::Other { mode });
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    WorkspaceSnapshot {
        entries,
        git_status: git_status_snapshot(root),
    }
}

/// Whether a path is a large Git file the worktree status already covers.
fn git_skipped(relative: &Path) -> bool {
    let text = relative.to_string_lossy().replace('\\', "/");
    let Some((_before, after)) = text.split_once(".git/") else {
        return false;
    };
    after == "index"
        || after == "index.lock"
        || after == "objects"
        || after.starts_with("objects/")
        || after == "lfs"
        || after.starts_with("lfs/")
}

/// A stable digest of a file's content, used so a same-length edit with an
/// unchanged modified time is still visible.
fn digest_file(path: &Path) -> [u8; 32] {
    let Ok(mut file) = std::fs::File::open(path) else {
        return [0; 32];
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return [0; 32],
        }
    }
    hasher.finalize().into()
}

/// The worktree status of every repository a run can touch.
fn git_status_snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    for repository in push_repositories(root) {
        let relative = repository
            .strip_prefix(root)
            .unwrap_or(&repository)
            .to_path_buf();
        if let Ok(status) = run_git(&["status", "--porcelain=v1", "-z"], &repository, None) {
            out.insert(relative, status);
        }
    }
    out
}

/// Whether a changed `.git` path proves a local commit.
fn git_commit_fact(relative: &Path) -> bool {
    let text = relative.to_string_lossy().replace('\\', "/");
    let Some((_before, after)) = text.split_once(".git/") else {
        return false;
    };
    after == "HEAD" || after == "logs/HEAD" || after.starts_with("refs/heads/")
}

/// The changes since a before-picture, named for the run's records and screens.
pub fn changes_since(before: &WorkspaceSnapshot, root: &Path) -> WorkspaceChanges {
    let after = snapshot_workspace(root);
    let mut changes = WorkspaceChanges::default();
    let mut paths: Vec<&PathBuf> = before.entries.keys().chain(after.entries.keys()).collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        if before.entries.get(path) == after.entries.get(path) {
            continue;
        }
        let relative = path.to_string_lossy().replace('\\', "/");
        if relative.split('/').any(|component| component == ".git") {
            if git_commit_fact(path) {
                changes.committed = true;
            } else {
                changes.git_state = true;
            }
            continue;
        }
        changes.files.push(relative);
    }
    let mut repositories: Vec<&PathBuf> = before
        .git_status
        .keys()
        .chain(after.git_status.keys())
        .collect();
    repositories.sort();
    repositories.dedup();
    for repository in repositories {
        if before.git_status.get(repository) != after.git_status.get(repository) {
            changes.git_state = true;
        }
    }
    changes
}

/// A read-only snapshot of the remote-tracking refs a run can move.
///
/// The product offers `P` (open the pull request) only when the run really pushed a
/// branch, and a command's text is not evidence: `echo git push` is a successful tool
/// call that pushed nothing. The evidence is the repository's own state — the
/// remote-tracking refs read before and after the run, and git's reflog for the ref,
/// which records whether the update was a push or a fetch (the push-is-a-fact rule).
/// A repository that cannot be read is simply absent from the watch, so the screen
/// makes no push claim rather than guessing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushWatch {
    repositories: Vec<PushSnapshot>,
    /// The instant the before-picture was read, in seconds since the Unix epoch.
    ///
    /// A reflog entry written before the run says nothing about the run, so the
    /// witness accepts only an entry stamped at or after this instant.
    started_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PushSnapshot {
    root: PathBuf,
    remotes: BTreeMap<String, String>,
}

/// The repositories a run may use: the workspace itself and the clones one level
/// below it (the layout `g` creates).
fn push_repositories(workspace: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if workspace.join(".git").exists() {
        found.push(workspace.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(workspace) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') || !path.is_dir() {
                continue;
            }
            if path.join(".git").exists() {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Read a repository's local branches and remote-tracking refs.
fn refs_of(repository: &Path) -> Option<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let text = run_git(
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/remotes",
        ],
        repository,
        None,
    )
    .ok()?;
    let mut heads = BTreeMap::new();
    let mut remotes = BTreeMap::new();
    for line in text.lines() {
        let Some((name, object)) = line.trim().split_once(' ') else {
            continue;
        };
        if name.starts_with("refs/heads/") {
            heads.insert(name.to_owned(), object.to_owned());
        } else if name.starts_with("refs/remotes/") {
            remotes.insert(name.to_owned(), object.to_owned());
        }
    }
    Some((heads, remotes))
}

/// Take the before-picture of every repository a run can touch.
pub fn watch_push(workspace: &Path) -> PushWatch {
    let mut repositories = Vec::new();
    for root in push_repositories(workspace) {
        if let Some((_, remotes)) = refs_of(&root) {
            repositories.push(PushSnapshot { root, remotes });
        }
    }
    PushWatch {
        repositories,
        started_unix: (crate::time::now_unix_ms() / 1000) as i64,
    }
}

/// The branch a run pushed, when git's own record proves the direction.
///
/// A push and a fetch both move a remote-tracking ref, and a pull or fetch can land
/// that ref on the local branch's commit without pushing anything. Object equality
/// is therefore not evidence. Git writes the reason for each ref update into the
/// ref's reflog: a push records exactly `update by push`, while a fetch or pull
/// records its own action and status. The witness accepts the latest update of a
/// changed remote-tracking ref only when that update's message is exactly
/// `update by push`, its new object is the local branch's commit, and it is stamped
/// at or after the watch. The entry's old object is not compared with the
/// before-picture: a pull or an earlier push in the same run may precede the push
/// that produced the current object. A reflog that is absent or unreadable leaves no
/// evidence, so no claim is made.
pub fn pushed_during(watch: &PushWatch) -> Option<PushedRef> {
    for snapshot in &watch.repositories {
        let Some((heads, remotes)) = refs_of(&snapshot.root) else {
            continue;
        };
        for (refname, object) in &remotes {
            if snapshot.remotes.get(refname) == Some(object) {
                continue;
            }
            let Some(short) = refname.strip_prefix("refs/remotes/") else {
                continue;
            };
            let Some((_remote, branch)) = short.split_once('/') else {
                continue;
            };
            if branch == "HEAD" {
                continue;
            }
            if heads.get(&format!("refs/heads/{branch}")) != Some(object) {
                continue;
            }
            if reflog_records_push(&snapshot.root, refname, object, watch.started_unix) {
                return Some(PushedRef {
                    branch: branch.to_owned(),
                    remote_ref: refname.clone(),
                    repository: snapshot.root.clone(),
                });
            }
        }
    }
    None
}

/// One branch a run pushed, with the clone whose refs proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedRef {
    /// The local branch the pushed remote-tracking ref corresponds to.
    pub branch: String,
    /// The remote-tracking ref whose object changed.
    pub remote_ref: String,
    /// The clone the push was read from, so the witness can name its repository.
    pub repository: PathBuf,
}

/// Whether git's reflog for `refname` says the run pushed it.
///
/// Only the last entry can describe the update that produced the ref's current
/// object. It must name that object, have exactly `update by push` as its message,
/// and be stamped at or after `started_unix`. A missing, unreadable or unparsable
/// reflog is not evidence.
fn reflog_records_push(repository: &Path, refname: &str, object: &str, started_unix: i64) -> bool {
    let Ok(git_dir) = run_git(&["rev-parse", "--absolute-git-dir"], repository, None) else {
        return false;
    };
    let log = Path::new(git_dir.trim()).join("logs").join(refname);
    let Ok(text) = std::fs::read_to_string(log) else {
        return false;
    };
    let Some(line) = text.lines().next_back() else {
        return false;
    };
    let Some((header, message)) = line.split_once('\t') else {
        return false;
    };
    let fields: Vec<&str> = header.split_whitespace().collect();
    if fields.len() < 4 {
        return false;
    }
    let new = fields[1];
    let stamp = fields[fields.len() - 2].parse::<i64>().ok();
    new == object && message == "update by push" && stamp.is_some_and(|stamp| stamp >= started_unix)
}

/// The environment git is given so it authenticates as the agent.
///
/// The values are a path, a name and a login. The token is not among them; the
/// program git calls reads it from the credentials file when git asks.
pub fn git_env(credential: &GitCredential<'_>) -> Vec<(String, String)> {
    vec![
        (
            "GIT_ASKPASS".to_owned(),
            credential.askpass.display().to_string(),
        ),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        (
            ASKPASS_CREDENTIALS.to_owned(),
            credential.credentials.display().to_string(),
        ),
        (ASKPASS_AGENT.to_owned(), credential.agent.to_owned()),
        (ASKPASS_LOGIN.to_owned(), credential.login.to_owned()),
    ]
}

/// Run one git command, without a secret anywhere in it.
fn run_git(
    args: &[&str],
    cwd: &Path,
    credential: Option<&GitCredential<'_>>,
) -> Result<String, WorkspaceError> {
    let mut command = Command::new("git");
    // No credential helper: the token travels through the askpass pipe and is never
    // written into a git credential store.
    command.arg("-c").arg("credential.helper=");
    command.args(args);
    command.current_dir(cwd);
    command.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(credential) = credential {
        for (key, value) in git_env(credential) {
            command.env(key, value);
        }
    }
    let output = command
        .output()
        .map_err(|source| WorkspaceError::GitMissing { source })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WorkspaceError::Git {
            command: format!("git {}", args.join(" ")),
            message: crate::model::excerpt(stderr.trim(), 500),
        });
    }
    Ok(stdout)
}

/// The askpass entry point: hand git the login or the token it asked for.
///
/// Git calls this program with the prompt as the one argument. A prompt asking for a
/// user name is answered with the login; anything else — a password or a passphrase —
/// with the agent's token, read from the credentials file named in the environment.
/// Nothing is written to standard error, so a failure cannot put a secret in a log.
///
/// The return value is the process exit code: zero when an answer was written, one
/// otherwise.
pub fn askpass_main(prompt: &str) -> i32 {
    let credentials_path = match std::env::var(ASKPASS_CREDENTIALS) {
        Ok(path) => PathBuf::from(path),
        Err(_) => return 1,
    };
    let agent = match std::env::var(ASKPASS_AGENT) {
        Ok(agent) => agent,
        Err(_) => return 1,
    };
    let answer = if prompt.to_ascii_lowercase().contains("username") {
        std::env::var(ASKPASS_LOGIN).ok().or_else(|| {
            credentials::recorded_login(&credentials_path, &agent)
                .ok()
                .flatten()
        })
    } else {
        credentials::forgejo_agent_token(&credentials_path, &agent)
            .ok()
            .map(|token| token.expose().to_owned())
    };
    let Some(answer) = answer else {
        return 1;
    };
    let mut out = std::io::stdout();
    if out.write_all(answer.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
        return 1;
    }
    if out.flush().is_err() {
        return 1;
    }
    0
}

impl fmt::Debug for GitCredential<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitCredential")
            .field("askpass", &self.askpass)
            .field("credentials", &self.credentials)
            .field("agent", &self.agent)
            .field("login", &self.login)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_url_carries_the_login_and_never_a_token() {
        let url = clone_url("https://forgejo.example.org", "root/demo", "gewu-scribe");
        assert_eq!(url, "https://gewu-scribe@forgejo.example.org/root/demo.git");
        assert!(!url.contains("token"));
    }

    #[test]
    fn a_clone_destination_is_the_repository_name_inside_the_workspace() {
        assert_eq!(
            clone_destination(Path::new("/w"), "root/demo"),
            Path::new("/w/demo")
        );
    }

    #[test]
    fn the_git_environment_names_a_path_a_name_and_a_login_only() {
        let env = git_env(&GitCredential {
            askpass: Path::new("/usr/bin/gewu-multiverse"),
            credentials: Path::new("/home/me/.config/gewu-multiverse/credentials.toml"),
            agent: "scribe",
            login: "gewu-scribe",
        });
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(map["GEWU_GIT_AGENT"], "scribe");
        assert_eq!(map["GEWU_GIT_LOGIN"], "gewu-scribe");
        assert_eq!(map["GIT_ASKPASS"], "/usr/bin/gewu-multiverse");
        for value in map.values() {
            assert!(!value.contains("token"), "{value}");
        }
    }

    #[test]
    fn the_askpass_mode_writes_nothing_without_its_environment() {
        // No environment is set in the test process, so nothing is written and the
        // exit code says so.
        let code = askpass_main("Password for 'https://example.org':");
        assert_eq!(code, 1);
    }

    /// Run one git command in `dir`, failing the test if git refuses.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn a_workspace_with_history_reports_its_branch_commit_and_identity() {
        // W-2: a workspace that already has history was only ever a path on the Agent
        // screen. What the product can read from the repository is read; a directory
        // that is not a repository is said to be one, not guessed at.
        let root = crate::test_support::scratch_root("workspace-inspect");
        let repository = root.join("repo");
        std::fs::create_dir_all(&repository).expect("the repository directory");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "gewu-scribe"]);
        git(
            &repository,
            &["config", "user.email", "gewu-scribe@agents.invalid"],
        );
        std::fs::write(repository.join("README"), "mine\n").expect("a file");
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-q", "-m", "mine"]);
        let state = inspect(&repository)
            .expect("git runs")
            .expect("a repository");
        assert_eq!(state.branch, "main");
        assert!(!state.commit.is_empty(), "the commit is named");
        assert!(state.committed_at > 0, "the commit has a time");
        assert_eq!(state.changed, 0, "the fresh commit is clean");
        assert_eq!(
            state.local_identity,
            Some("gewu-scribe <gewu-scribe@agents.invalid>".to_owned())
        );
        // A directory that is not a repository is a state, not an error.
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).expect("the plain directory");
        assert_eq!(inspect(&plain).expect("git is not even asked"), None);
        // A change is counted.
        std::fs::write(repository.join("README"), "changed\n").expect("an edit");
        let state = inspect(&repository)
            .expect("git runs")
            .expect("a repository");
        assert_eq!(state.changed, 1);
    }

    #[test]
    fn a_destination_that_already_holds_a_repository_is_reported_as_kept_not_cloned() {
        // F2: `g` on a directory that already held a git repository reported `is
        // ready` without cloning, fetching, or saying it had kept what was there. The
        // outcome now says which act happened, so no surface calls a reuse a clone.
        let root = crate::test_support::scratch_root("workspace-reuse");
        let workspace = root.join("workspace");
        let destination = workspace.join("demo");
        std::fs::create_dir_all(&destination).expect("the destination is made");
        git(&destination, &["init", "-q"]);
        std::fs::write(destination.join("README"), "mine\n").expect("a file");
        git(&destination, &["add", "."]);
        git(
            &destination,
            &[
                "-c",
                "user.name=me",
                "-c",
                "user.email=me@example.org",
                "commit",
                "-q",
                "-m",
                "mine",
            ],
        );
        let identity = GitIdentity {
            login: "gewu-scribe".to_owned(),
            email: "gewu-scribe@agents.invalid".to_owned(),
        };
        let (path, outcome) = prepare(
            "https://forgejo.example.org",
            "migrator/gewu-multiverse",
            &workspace,
            &destination,
            &identity,
            None,
        )
        .expect("the existing repository is kept");
        assert_eq!(path, destination);
        assert_eq!(outcome, Prepared::AlreadyPresent);
        // Nothing was cloned or fetched, and the tool's identity was applied locally.
        let remotes = Command::new("git")
            .args(["remote", "-v"])
            .current_dir(&destination)
            .output()
            .expect("git runs");
        assert!(
            String::from_utf8_lossy(&remotes.stdout).trim().is_empty(),
            "a remote was added"
        );
        let name = Command::new("git")
            .args(["config", "user.name"])
            .current_dir(&destination)
            .output()
            .expect("git runs");
        assert_eq!(String::from_utf8_lossy(&name.stdout).trim(), "gewu-scribe");
        // A non-empty directory that is *not* a repository is still refused.
        let plain = workspace.join("plain");
        std::fs::create_dir_all(&plain).expect("the plain directory");
        std::fs::write(plain.join("file"), "x").expect("a file");
        let error = prepare(
            "https://forgejo.example.org",
            "migrator/plain",
            &workspace,
            &plain,
            &identity,
            None,
        )
        .expect_err("a non-git directory is refused");
        assert_eq!(error.class(), "destination_in_the_way");
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn a_git_refusal_leads_with_gits_own_message() {
        // N-6: a failed clone must say why, not lead with the long command it ran.
        let error = WorkspaceError::Git {
            command: "git clone http://example.invalid/very/long/path.git /tmp/ws".to_owned(),
            message: "repository not found".to_owned(),
        };
        let detail = error.detail();
        assert!(detail.starts_with("repository not found"), "{detail}");
        assert!(!detail.starts_with("`git"), "{detail}");
    }
}

#[cfg(test)]
mod change_tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::fs;

    fn changed(root: &Path, change: impl FnOnce()) -> WorkspaceChanges {
        let before = snapshot_workspace(root);
        change();
        changes_since(&before, root)
    }

    #[test]
    fn a_mode_only_change_is_local_work() {
        let root = scratch_root("workspace-mode");
        let file = root.join("README.md");
        fs::write(&file, "hello\n").expect("the file");
        let changes = changed(&root, || {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("the mode");
        });
        assert_eq!(changes.files, vec!["README.md".to_owned()]);
        assert_eq!(
            changes.work_line().as_deref(),
            Some("local change README.md")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_same_length_same_time_content_edit_is_local_work() {
        let root = scratch_root("workspace-content");
        let file = root.join("README.md");
        fs::write(&file, "aaaa\n").expect("the file");
        let old = fs::metadata(&file)
            .and_then(|meta| meta.modified())
            .expect("the old time");
        let changes = changed(&root, || {
            fs::write(&file, "bbbb\n").expect("the edit");
            let handle = fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .expect("the file opens");
            handle
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .expect("the time is restored");
        });
        assert_eq!(changes.files, vec!["README.md".to_owned()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_symlink_and_an_empty_directory_are_local_work() {
        let root = scratch_root("workspace-links");
        let changes = changed(&root, || {
            fs::create_dir(root.join("empty")).expect("the empty directory");
            std::os::unix::fs::symlink("target", root.join("link")).expect("the symlink");
        });
        assert!(changes.files.contains(&"empty".to_owned()), "{changes:?}");
        assert!(changes.files.contains(&"link".to_owned()), "{changes:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_staged_change_is_local_git_state() {
        let root = scratch_root("workspace-staged");
        git(&root, &["init", "-q", "--initial-branch=main"]);
        fs::write(root.join("README.md"), "hello\n").expect("the file");
        git(&root, &["add", "README.md"]);
        git(
            &root,
            &[
                "-c",
                "user.name=gewu test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "first",
            ],
        );
        let changes = changed(&root, || {
            fs::write(root.join("staged.txt"), "staged\n").expect("the staged file");
            git(&root, &["add", "staged.txt"]);
        });
        assert!(changes.git_state, "{changes:?}");
        assert!(changes.work_line().is_some(), "{changes:?}");
        let _ = fs::remove_dir_all(&root);
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
