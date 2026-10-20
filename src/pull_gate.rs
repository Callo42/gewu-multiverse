//! The rules a pull-request write must pass, shared by the person's keys and the tools.
//!
//! `P` and `U` are the person's keys on the Forgejo screen; the model has
//! `forgejo_open_pull_request` and `forgejo_update_pull_request`. Both roads use the
//! rules in this module, so a branch that `P` refuses cannot be opened by a tool and a
//! request `U` would never touch cannot be updated by one:
//!
//! - **open**: the checked-out branch is not the base, it has commits the base lacks
//!   (measured against the remote-tracking base when the clone has one), and its
//!   remote-tracking ref exists in this clone;
//! - **the pair**: a matching open request has this branch as head, this base as base,
//!   and this repository as the head's repository; a head that names no repository is
//!   not a match;
//! - **authorship**: only an author the instance names and that equals the agent's
//!   recorded login is the agent's own. A named author with no recorded login cannot
//!   be compared, which is its own fact — not "another account's".
//!
//! This module only reads git and values it is given. It never writes, never makes a
//! network call and never holds a secret.

use std::path::Path;

use crate::forgejo::PullRequest;
use crate::workspace;

/// Whose open request a matching pair belongs to.
///
/// The rule is deliberately conservative: only an author the instance names and that
/// exactly equals the agent's own recorded login is the agent's own. A named author
/// with no recorded login cannot be compared; an unnamed author is its own fact. The
/// same answer decides the refusal route on `P`, `U`, the Runs note, the tools and an
/// after-write conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestAuthorship {
    /// The request names the agent's own recorded login as its author.
    Own,
    /// The request names a different account.
    AnotherAccount,
    /// The request names an account, but the agent has no recorded login to compare.
    Uncompared,
    /// The request names no author at all.
    UnnamedAuthor,
}

impl PullRequestAuthorship {
    /// Classify one request's author against the agent's recorded login.
    ///
    /// A missing author is always [`Self::UnnamedAuthor`], whether or not a login was
    /// recorded. A named author with no login is [`Self::Uncompared`], never
    /// [`Self::AnotherAccount`]: the product cannot prove "another account" from a
    /// value it never read.
    pub fn of(author: Option<&str>, login: Option<&str>) -> Self {
        match (author, login) {
            (Some(author), Some(login)) if author == login => Self::Own,
            (Some(_), Some(_)) => Self::AnotherAccount,
            (Some(_), None) => Self::Uncompared,
            (None, _) => Self::UnnamedAuthor,
        }
    }

    /// Whether this request is the agent's own, and so what the update may touch.
    pub fn is_own(self) -> bool {
        self == Self::Own
    }

    /// What the request is, in the words every surface uses.
    pub fn fact(self) -> &'static str {
        match self {
            Self::Own => "it is the agent's own request",
            Self::AnotherAccount => "it is another account's request",
            Self::Uncompared => "its author cannot be compared",
            Self::UnnamedAuthor => "its author is not named",
        }
    }

    /// The one route that would update this request, or why there is none.
    pub fn update_route(self) -> &'static str {
        match self {
            Self::Own => "press U on the Forgejo screen (4) to update it",
            Self::AnotherAccount | Self::Uncompared | Self::UnnamedAuthor => {
                "U updates only your own request, so nothing was written"
            }
        }
    }
}

/// Whether one open request is for this repository's branch pair.
///
/// A head that names no repository is not a match: the product cannot prove that the
/// request comes from this clone's repository, so it must not act on it.
pub fn matches_pair(pull: &PullRequest, repository: &str, branch: &str, base: &str) -> bool {
    pull.head == branch && pull.base == base && pull.head_repo.as_deref() == Some(repository)
}

/// Put the agent's own request before any other request on the same branch.
pub fn own_requests_first(mut pulls: Vec<PullRequest>, login: Option<&str>) -> Vec<PullRequest> {
    pulls.sort_by_key(
        |pull| match PullRequestAuthorship::of(pull.author.as_deref(), login) {
            PullRequestAuthorship::Own => 0u8,
            PullRequestAuthorship::AnotherAccount
            | PullRequestAuthorship::Uncompared
            | PullRequestAuthorship::UnnamedAuthor => 1u8,
        },
    );
    pulls
}

/// Check that this clone has something a pull request could contain.
///
/// The three content facts, and nothing about identity or a token: the caller adds
/// those, because `P` reads them from the credentials file while a tool reads them
/// from the context of its run. The refusal is one sentence without the key's name,
/// so each caller can put its own key or tool name in front.
pub fn content_gate(destination: &Path, branch: &str, base: &str) -> Result<(), String> {
    if branch == base {
        return Err(format!(
            "`{branch}` is the base branch, so there is nothing to open as a pull request"
        ));
    }
    let (ahead, measured_base) = workspace::commits_ahead_against(destination, branch, base)
        .map_err(|error| {
            format!(
                "the commits `{branch}` has beyond `{base}` could not be read ({}): {}",
                error.class(),
                error.detail()
            )
        })?;
    if ahead == 0 {
        return Err(format!(
            "`{branch}` has no commits `{measured_base}` lacks, so there is nothing to open"
        ));
    }
    // The selected clone's own remote-tracking ref is the evidence that the branch is
    // on the remote. A run's push witness is about the run's workspace, not this
    // clone: a sibling clone's push must not stand for this branch.
    if !workspace::branch_on_remote(destination, branch) {
        return Err(format!("`{branch}` is not on the remote — push it first"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(author: Option<&str>, head_repo: Option<&str>) -> PullRequest {
        PullRequest {
            number: 7,
            title: "a request".to_owned(),
            state: "open".to_owned(),
            html_url: None,
            head: "topic".to_owned(),
            head_repo: head_repo.map(str::to_owned),
            base: "main".to_owned(),
            base_repo: Some("lab/demo".to_owned()),
            author: author.map(str::to_owned),
        }
    }

    #[test]
    fn a_named_author_without_a_recorded_login_cannot_be_compared() {
        assert_eq!(
            PullRequestAuthorship::of(Some("someone"), None),
            PullRequestAuthorship::Uncompared
        );
        assert_eq!(
            PullRequestAuthorship::Uncompared.fact(),
            "its author cannot be compared"
        );
        assert_ne!(
            PullRequestAuthorship::Uncompared.fact(),
            PullRequestAuthorship::AnotherAccount.fact()
        );
    }

    #[test]
    fn only_an_exact_recorded_login_is_own() {
        assert_eq!(
            PullRequestAuthorship::of(Some("gewu-scribe"), Some("gewu-scribe")),
            PullRequestAuthorship::Own
        );
        assert_eq!(
            PullRequestAuthorship::of(Some("someone-else"), Some("gewu-scribe")),
            PullRequestAuthorship::AnotherAccount
        );
        assert_eq!(
            PullRequestAuthorship::of(None, Some("gewu-scribe")),
            PullRequestAuthorship::UnnamedAuthor
        );
    }

    #[test]
    fn a_pair_needs_the_same_repository_branch_and_base() {
        assert!(matches_pair(
            &pull(Some("a"), Some("lab/demo")),
            "lab/demo",
            "topic",
            "main"
        ));
        assert!(!matches_pair(
            &pull(Some("a"), None),
            "lab/demo",
            "topic",
            "main"
        ));
        assert!(!matches_pair(
            &pull(Some("a"), Some("lab/other")),
            "lab/demo",
            "topic",
            "main"
        ));
        assert!(!matches_pair(
            &pull(Some("a"), Some("lab/demo")),
            "lab/demo",
            "other",
            "main"
        ));
        assert!(!matches_pair(
            &pull(Some("a"), Some("lab/demo")),
            "lab/demo",
            "topic",
            "dev"
        ));
    }
}
