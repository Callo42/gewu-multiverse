//! The Forgejo integration test, against a local throwaway instance.
//!
//! It runs only when `GEWU_FORGEJO_BIN` names a Forgejo server binary; when it does
//! not, it says so and does nothing. Everything it proves is proved against that one
//! local instance, with a temporary root, and nothing else on the machine is touched:
//!
//! 1. the role check under an administrator's token and under an ordinary token, with
//!    the difference — the private repositories only the administrator can see —
//!    visible;
//! 2. a bad URL, a bad token, and an ordinary token where the configuration expects an
//!    administrator, each with its own failure class;
//! 3. an agent identity created by the tool from an administrator token, and the token
//!    it makes stored in the credentials file, mode 0600;
//! 4. a commit, made in the agent's workspace, authored by that identity — the
//!    author line is printed and asserted — pushed, and a pull request opened and
//!    updated as the agent;
//! 5. the non-administrator path: a token the user supplied, producing a commit with
//!    the same attribution;
//! 6. an operation outside the ordinary token's rights, refused, with the error the
//!    instance gave;
//! 7. the assertion that none of the tokens appears in anything the tool produces,
//!    including the askpass answer, which is the one place a token is meant to leave.
//!
//! Run it with the server binary named:
//!
//! ```text
//! GEWU_FORGEJO_BIN=/path/to/forgejo cargo test --test forgejo -- --nocapture
//! ```

mod forgejo_support;

use std::path::Path;
use std::process::Command;

use forgejo_support::TestForgejo;

use gewu_multiverse::config::Role;
use gewu_multiverse::credentials::{self, Secret};
use gewu_multiverse::forgejo::{self, DEFAULT_TIMEOUT, ForgejoClient, ForgejoConfig};
use gewu_multiverse::identity::{self, IdentityMode};
use gewu_multiverse::mode;
use gewu_multiverse::workspace::{self, GitCredential, GitIdentity};

/// The password of the test administrator. It is a throwaway for a throwaway
/// instance, and it is never printed.
const ADMIN_PASSWORD: &str = "Gw-Test-Admin-1!";
/// The password of the ordinary test account. Also a throwaway, never printed.
const AUTHOR_PASSWORD: &str = "Gw-Test-Author-1!";

/// The binary under test, which git calls back for a token.
fn askpass_binary() -> &'static str {
    env!("CARGO_BIN_EXE_gewu-multiverse")
}

/// Run one git command in a repository, and fail loudly if it refuses.
fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The author line of the newest commit in a repository.
fn commit_author(repository: &Path) -> String {
    git(repository, &["log", "-1", "--format=%an <%ae>"])
        .trim()
        .to_owned()
}

/// A client for one token, for the calls the test makes directly.
fn client_for(url: &str, token: &str) -> ForgejoClient {
    ForgejoClient::new(url, Secret::new(token), DEFAULT_TIMEOUT).expect("a client")
}

#[test]
fn the_forgejo_round_trip() {
    let Some(fj) = TestForgejo::start("round-trip") else {
        eprintln!(
            "GEWU_FORGEJO_BIN is not set, so the local Forgejo integration test was skipped; \
             nothing was verified by it."
        );
        return;
    };
    println!("local test Forgejo ready at {}", fj.url);

    // ---- an instance with an administrator, and two repositories ----
    fj.create_user("root", ADMIN_PASSWORD, true);
    let admin = fj.access_token_for("root");
    fj.create_repo(&admin, "demo", false);
    fj.create_repo(&admin, "secret-repo", true);

    // ---- the role check, administrative view ----
    let admin_config = ForgejoConfig::new(&fj.url, Secret::new(&admin));
    let administrative =
        forgejo::role_check(&admin_config, Role::Admin).expect("the administrator's check passes");
    println!("role check (admin): {}", administrative.summary());
    assert!(administrative.viewer.is_admin);
    assert!(administrative.administrative());
    assert!(
        administrative.private_visible >= 1,
        "an administrator sees the private repositories: {}",
        administrative.summary()
    );
    assert!(
        administrative
            .repositories
            .iter()
            .any(|repo| repo.full_name == "root/secret-repo")
    );
    assert!(
        administrative
            .repositories
            .iter()
            .any(|repo| repo.permissions.admin)
    );

    // ---- the role check, ordinary view ----
    fj.create_user("author", AUTHOR_PASSWORD, false);
    // The author's token is made through the tool itself, with the tool's own rights:
    // enough to read and to write a repository, and no more.
    let author_token = client_for(&fj.url, &admin)
        .create_token_for("author", "author-token")
        .expect("a token is made for the ordinary account");
    let author_config = ForgejoConfig::new(&fj.url, author_token.clone());
    let ordinary =
        forgejo::role_check(&author_config, Role::Member).expect("the member's check passes");
    println!("role check (member): {}", ordinary.summary());
    assert!(!ordinary.viewer.is_admin);
    assert!(!ordinary.administrative());
    // The difference is visible: the ordinary token cannot see the private
    // repository at all, while the administrator's can.
    assert!(
        !ordinary
            .repositories
            .iter()
            .any(|repo| repo.full_name == "root/secret-repo"),
        "an ordinary token must not see another account's private repository"
    );
    assert!(
        ordinary
            .repositories
            .iter()
            .any(|repo| repo.full_name == "root/demo")
    );
    assert_eq!(ordinary.private_visible, 0);
    println!(
        "difference: admin sees {} repository(ies) ({} private), member sees {} ({} private)",
        administrative.repositories.len(),
        administrative.private_visible,
        ordinary.repositories.len(),
        ordinary.private_visible
    );

    // ---- a token without the rights the configuration expects ----
    let not_admin =
        forgejo::role_check(&author_config, Role::Admin).expect_err("an ordinary token is refused");
    assert_eq!(not_admin.class(), "not_admin");
    println!(
        "role = admin with an ordinary token: ({}) {not_admin}",
        not_admin.class()
    );

    // ---- a bad URL and a bad token ----
    let bad_url = forgejo::role_check(
        &ForgejoConfig::new("forgejo.example.org", Secret::new(&admin)),
        Role::Member,
    )
    .expect_err("a URL without a scheme is refused");
    assert_eq!(bad_url.class(), "bad_url");
    println!("bad URL: ({}) {bad_url}", bad_url.class());

    let bad_token = forgejo::role_check(
        &ForgejoConfig::new(
            &fj.url,
            Secret::new("gw-not-a-real-token-00000000000000000000"),
        ),
        Role::Member,
    )
    .expect_err("a bad token is refused");
    assert_eq!(bad_token.class(), "bad_token");
    println!("bad token: ({}) {bad_token}", bad_token.class());

    // ---- one identity per agent, the administrator path ----
    let managed_root = forgejo_support::scratch("managed");
    let managed_credentials = managed_root.join("credentials.toml");
    mode::write_private(
        &managed_credentials,
        format!("forgejo_token = \"{admin}\"\nforgejo_admin_token = \"{admin}\"\n").as_bytes(),
    )
    .expect("the credentials file is written");
    let scribe = identity::manage(&fj.url, "scribe", &managed_credentials)
        .expect("the tool creates the agent's identity");
    println!("managed identity created: {}", scribe.line());
    assert_eq!(scribe.login, "gewu-scribe");
    assert!(scribe.created, "the account was created by the tool");
    assert_eq!(scribe.mode, IdentityMode::Managed);
    // The credentials file is held to 0600, and the token it now holds works.
    assert_eq!(
        mode::mode_of(&managed_credentials).expect("the mode is readable"),
        0o600
    );
    let scribe_token = credentials::forgejo_agent_token(&managed_credentials, "scribe")
        .expect("the identity's token is recorded");
    let scribe_client =
        ForgejoClient::new(&fj.url, scribe_token.clone(), DEFAULT_TIMEOUT).expect("a client");
    assert_eq!(
        scribe_client.viewer().expect("the token works").login,
        "gewu-scribe"
    );
    // A second manage call reuses the account and its token.
    let again = identity::manage(&fj.url, "scribe", &managed_credentials).expect("managed again");
    assert!(!again.created, "the account already existed");
    assert_eq!(again.login, "gewu-scribe");

    // The agent needs a repository of its own to make its commit in.
    fj.create_repo_for(&admin, "gewu-scribe", "demo", false);

    // ---- a commit authored by the agent's identity, and a pull request ----
    let workspace_root = forgejo_support::scratch("workspace-managed");
    let destination = workspace::clone_destination(&workspace_root, "gewu-scribe/demo");
    let identity = GitIdentity {
        login: scribe.login.clone(),
        email: scribe.email.clone(),
    };
    let credential = GitCredential {
        askpass: Path::new(askpass_binary()),
        credentials: &managed_credentials,
        agent: "scribe",
        login: &scribe.login,
    };
    workspace::prepare(
        &fj.url,
        "gewu-scribe/demo",
        &workspace_root,
        &destination,
        &identity,
        Some(&credential),
    )
    .expect("the repository is cloned into the workspace and the identity is configured");

    git(&destination, &["checkout", "-b", "note-as-scribe"]);
    std::fs::write(
        destination.join("note-as-scribe.txt"),
        "written by the agent\n",
    )
    .expect("the note is written");
    git(&destination, &["add", "-A"]);
    git(
        &destination,
        &["commit", "-m", "Add a note, authored by the agent"],
    );
    let author = commit_author(&destination);
    println!("commit author (managed path): {author}");
    assert_eq!(author, format!("{} <{}>", scribe.login, scribe.email));

    workspace::push(&destination, &credential).expect("the branch is pushed as the agent");
    println!("pushed branch note-as-scribe");

    let pull = scribe_client
        .open_pull_request(
            "gewu-scribe",
            "demo",
            "A note, as the agent",
            "Opened through the tool as the agent's own identity.",
            "note-as-scribe",
            "main",
        )
        .expect("the pull request is opened as the agent");
    println!("opened pull request {}", pull.line());
    assert_eq!(pull.state, "open");
    assert_eq!(pull.head, "note-as-scribe");
    assert_eq!(pull.base, "main");

    let updated = scribe_client
        .update_pull_request(
            "gewu-scribe",
            "demo",
            pull.number,
            Some("A note, as the agent (updated)"),
            Some("Updated through the tool as the agent's own identity."),
            None,
        )
        .expect("the pull request is updated as the agent");
    assert_eq!(updated.title, "A note, as the agent (updated)");
    println!("updated pull request {}", updated.line());

    // ---- the non-administrator path: a supplied token, same attribution ----
    fj.create_repo(author_token.expose(), "work", false);
    let supplied_root = forgejo_support::scratch("supplied");
    let supplied_credentials = supplied_root.join("credentials.toml");
    mode::write_private(
        &supplied_credentials,
        format!(
            "[forgejo_agent_tokens]\n\"scribe\" = \"{}\"\n",
            author_token.expose()
        )
        .as_bytes(),
    )
    .expect("the supplied credentials file is written");
    let supplied = identity::resolve(&fj.url, "scribe", &supplied_credentials)
        .expect("the supplied token settles the identity");
    println!("supplied identity: {}", supplied.line());
    assert_eq!(supplied.login, "author");
    assert_eq!(supplied.mode, IdentityMode::Supplied);
    assert!(!supplied.created);

    let supplied_workspace = forgejo_support::scratch("workspace-supplied");
    let supplied_destination = workspace::clone_destination(&supplied_workspace, "author/work");
    let supplied_identity = GitIdentity {
        login: supplied.login.clone(),
        email: supplied.email.clone(),
    };
    let supplied_credential = GitCredential {
        askpass: Path::new(askpass_binary()),
        credentials: &supplied_credentials,
        agent: "scribe",
        login: &supplied.login,
    };
    workspace::prepare(
        &fj.url,
        "author/work",
        &supplied_workspace,
        &supplied_destination,
        &supplied_identity,
        Some(&supplied_credential),
    )
    .expect("the second repository is cloned under the supplied identity");
    git(&supplied_destination, &["checkout", "-b", "note-as-author"]);
    std::fs::write(
        supplied_destination.join("note.txt"),
        "written by the supplied agent\n",
    )
    .expect("the note is written");
    git(&supplied_destination, &["add", "-A"]);
    git(
        &supplied_destination,
        &[
            "commit",
            "-m",
            "Add a note, authored by the supplied identity",
        ],
    );
    let supplied_author = commit_author(&supplied_destination);
    println!("commit author (supplied path): {supplied_author}");
    assert_eq!(
        supplied_author,
        format!("{} <{}>", supplied.login, supplied.email)
    );
    workspace::push(&supplied_destination, &supplied_credential)
        .expect("the branch is pushed under the supplied token");
    let supplied_pull = client_for(&fj.url, author_token.expose())
        .open_pull_request(
            "author",
            "work",
            "A note, as the supplied agent",
            "Opened with the token the user supplied.",
            "note-as-author",
            "main",
        )
        .expect("the pull request is opened under the supplied token");
    println!("opened pull request {}", supplied_pull.line());
    assert_eq!(supplied_pull.state, "open");

    // ---- an operation outside the ordinary token's rights, refused ----
    let refused = client_for(&fj.url, author_token.expose())
        .create_user("never-created", "never-created@example.org", "Gw-Never-1!")
        .expect_err("an ordinary token cannot create an account");
    assert_eq!(refused.class(), "forbidden");
    println!(
        "operation outside the ordinary token's rights refused: ({}) {refused}",
        refused.class()
    );
    assert!(refused.to_string().contains("write:admin"), "{refused}");

    // ---- no token in anything the tool produces ----
    let surroundings = vec![
        administrative.summary(),
        ordinary.summary(),
        not_admin.to_string(),
        bad_url.to_string(),
        bad_token.to_string(),
        scribe.line(),
        supplied.line(),
        pull.line(),
        updated.line(),
        supplied_pull.line(),
        refused.to_string(),
        format!("{admin_config:?}"),
        format!("{author_config:?}"),
        format!("{scribe_client:?}"),
        format!("{credential:?}"),
        format!("{supplied_credential:?}"),
    ];
    for text in &surroundings {
        for token in [admin.as_str(), author_token.expose(), scribe_token.expose()] {
            assert!(!text.contains(token), "a token appears in output: {text}");
        }
    }
    println!("no token appears in any message, line or client the tool produced");

    // The credentials file records the login and the email, and stays private.
    let recorded = credentials::recorded_login(&managed_credentials, "scribe")
        .expect("the file reads")
        .expect("a login is recorded");
    assert_eq!(recorded, "gewu-scribe");
    assert_eq!(
        credentials::recorded_identity_mode(&managed_credentials)
            .expect("the file reads")
            .as_deref(),
        Some("managed")
    );
    assert_eq!(
        credentials::recorded_identity_mode(&supplied_credentials)
            .expect("the file reads")
            .as_deref(),
        Some("supplied")
    );
    assert_eq!(
        mode::mode_of(&supplied_credentials).expect("the mode is readable"),
        0o600
    );

    // ---- the one place a token leaves: the askpass pipe, and only there ----
    let askpass = askpass_binary();
    for (prompt, expected) in [
        (
            "Password for 'http://gewu-scribe@127.0.0.1':",
            scribe_token.expose().to_owned(),
        ),
        ("Username for 'http://127.0.0.1':", "gewu-scribe".to_owned()),
    ] {
        let output = Command::new(askpass)
            .arg("--git-askpass")
            .arg(prompt)
            .env("GEWU_GIT_CREDENTIALS", &managed_credentials)
            .env("GEWU_GIT_AGENT", "scribe")
            .env("GEWU_GIT_LOGIN", "gewu-scribe")
            .output()
            .expect("the askpass mode runs");
        assert!(output.status.success(), "the askpass answer {prompt}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), expected, "the askpass answer for {prompt}");
        assert!(
            output.stderr.is_empty(),
            "the askpass mode wrote to standard error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!("the askpass mode answers on standard output only, and nothing on standard error");

    println!("the Forgejo round trip passed");
}
