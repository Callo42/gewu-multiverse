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

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

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
/// it is dirty, or whose identity its commits carry (W-2). Every field here is read
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
    let committed_at = head_lines
        .next()
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
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

/// Push the current branch to `origin`, as the agent.
pub fn push(repository: &Path, credential: &GitCredential<'_>) -> Result<(), WorkspaceError> {
    run_git(
        &["push", "--set-upstream", "origin", "HEAD"],
        repository,
        Some(credential),
    )?;
    Ok(())
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
        git(&repository, &["config", "user.email", "gewu-scribe@agents.invalid"]);
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
        let state = inspect(&repository).expect("git runs").expect("a repository");
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
