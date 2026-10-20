//! The Forgejo integration test, against a local throwaway instance.
//!
//! It runs only when `GEWU_FORGEJO_BIN` names a Forgejo server binary. The tests are
//! `#[ignore]`d, so the ordinary suite lists them as ignored rather than counting them
//! as passed; they are run explicitly and fail when the variable is absent. Everything
//! it proves is proved against that one local instance, with a temporary root, and
//! nothing else on the machine is touched:
//!
//! 1. a token's account and the repositories it can see, including the private
//!    repository an administrator can see and an ordinary account cannot;
//! 2. a URL without a scheme and a token the instance refuses, each with its own
//!    failure class;
//! 3. an agent identity created by the tool from the administrator token, and the
//!    token it makes stored in the credentials file, mode 0600;
//! 4. a commit, made in the agent's workspace, authored by that identity — the
//!    author line is printed and asserted — pushed, and a pull request opened and
//!    updated as the agent;
//! 5. an operation outside the ordinary token's rights, refused, with the error the
//!    instance gave;
//! 6. the assertion that none of the tokens appears in anything the tool produces,
//!    including the askpass answer, which is the one place a token is meant to leave.
//!
//! Run it with the server binary named:
//!
//! ```text
//! GEWU_FORGEJO_BIN=/path/to/forgejo cargo test --test forgejo -- --ignored --nocapture
//! ```

mod forgejo_support;

use std::fs;
use std::path::Path;
use std::process::Command;

use forgejo_support::TestForgejo;

use gewu_multiverse::credentials::{self, Secret};
use gewu_multiverse::forgejo::{DEFAULT_TIMEOUT, ForgejoClient};
use gewu_multiverse::identity;
use gewu_multiverse::mode;
use gewu_multiverse::run::RunPlan;
use gewu_multiverse::test_stub::{Stub, StubReply};
use gewu_multiverse::transcript;
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
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BIN=... cargo test --test forgejo -- --ignored"]
fn the_forgejo_round_trip() {
    let fj = TestForgejo::start("round-trip");
    println!("local test Forgejo ready at {}", fj.url);

    // ---- an instance with an administrator, and two repositories ----
    fj.create_user("root", ADMIN_PASSWORD, true);
    let admin = fj.access_token_for("root");
    fj.create_repo(&admin, "demo", false);
    fj.create_repo(&admin, "secret-repo", true);

    // ---- the account and the repositories it can see ----
    let admin_client = client_for(&fj.url, &admin);
    let administrative = admin_client
        .viewer()
        .expect("the administrator's token works");
    assert!(administrative.is_admin);
    let admin_repositories = admin_client
        .repositories()
        .expect("the repositories are read");
    assert!(
        admin_repositories
            .iter()
            .any(|repo| repo.full_name == "root/secret-repo"),
        "an administrator sees the private repositories"
    );
    assert!(admin_repositories.iter().any(|repo| repo.permissions.admin));

    // An ordinary account's token sees the public repository and not another
    // account's private one.
    fj.create_user("author", AUTHOR_PASSWORD, false);
    let author_token = client_for(&fj.url, &admin)
        .create_token_for("author", "author-token")
        .expect("a token is made for the ordinary account");
    let ordinary_client = client_for(&fj.url, author_token.expose());
    let ordinary = ordinary_client.viewer().expect("the ordinary token works");
    assert!(!ordinary.is_admin);
    let ordinary_repositories = ordinary_client
        .repositories()
        .expect("the ordinary account's repositories are read");
    assert!(
        !ordinary_repositories
            .iter()
            .any(|repo| repo.full_name == "root/secret-repo"),
        "an ordinary token must not see another account's private repository"
    );
    assert!(
        ordinary_repositories
            .iter()
            .any(|repo| repo.full_name == "root/demo")
    );
    println!(
        "admin sees {} repository(ies), member sees {}",
        admin_repositories.len(),
        ordinary_repositories.len()
    );

    // ---- a bad URL and a bad token ----
    let bad_url = ForgejoClient::new("forgejo.example.org", Secret::new(&admin), DEFAULT_TIMEOUT)
        .expect_err("a URL without a scheme is refused");
    assert_eq!(bad_url.class(), "bad_url");
    println!("bad URL: ({}) {bad_url}", bad_url.class());

    let bad_token = client_for(&fj.url, "gw-not-a-real-token-00000000000000000000")
        .viewer()
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

    // ---- an operation outside the ordinary token's rights, refused ----
    let refused = client_for(&fj.url, author_token.expose())
        .create_user("never-created", "never-created@example.org", "Gw-Never-1!")
        .expect_err("an ordinary token cannot create an account");
    // Forgejo names the missing scope in the 403 body; the product keeps the right
    // and never the rest of the body.
    assert_eq!(refused.class(), "missing_scope");
    println!(
        "operation outside the ordinary token's rights refused: ({}) {refused}",
        refused.class()
    );
    assert!(refused.to_string().contains("write:admin"), "{refused}");

    // ---- no token in anything the tool produces ----
    let surroundings = vec![
        bad_url.to_string(),
        bad_token.to_string(),
        scribe.line(),
        pull.line(),
        updated.line(),
        refused.to_string(),
        format!("{admin_client:?}"),
        format!("{ordinary_client:?}"),
        format!("{scribe_client:?}"),
        format!("{credential:?}"),
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

/// The six tools against the real instance, through the real binary and run loop.
///
/// This is the bounded real-Forgejo fidelity test: the same run the product would
/// make, with the model's calls scripted by a loopback stub and every Forgejo call
/// going to the throwaway instance. It is `#[ignore]`d and run explicitly with a server
/// binary named.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BIN=... cargo test --test forgejo -- --ignored"]
fn the_agent_tools_round_trip_against_a_live_instance() {
    let fj = TestForgejo::start("tools");
    fj.create_user("root", ADMIN_PASSWORD, true);
    let admin = fj.access_token_for("root");

    let root = forgejo_support::scratch("tools-run");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        format!("forgejo_token = \"{admin}\"\nforgejo_admin_token = \"{admin}\"\n").as_bytes(),
    )
    .expect("the credentials file");
    let scribe = identity::manage(&fj.url, "scribe", &credentials)
        .expect("the tool creates the agent's identity");
    let agent_token = credentials::forgejo_agent_token(&credentials, "scribe")
        .expect("the agent token is recorded");
    let client = ForgejoClient::new(&fj.url, agent_token.clone(), DEFAULT_TIMEOUT)
        .expect("a client for the agent");
    client
        .assigned_issues()
        .expect("a managed token can read issues");

    // The agent's own repository, with an issue assigned to its account.
    fj.create_repo_for(&admin, &scribe.login, "demo", false);
    let (status, text) = fj.api(
        &admin,
        "POST",
        &format!("/api/v1/repos/{}/demo/issues", scribe.login),
        Some(
            serde_json::json!({
                "title": "Work for the agent",
                "body": "please do it",
                "assignees": [scribe.login]
            })
            .to_string()
            .as_str(),
        ),
    );
    assert!(
        (200..300).contains(&status),
        "issue create {status}: {text}"
    );

    // A clone the product made, on a branch that really is pushed to the instance.
    let workspace_root = root.join("workspace");
    fs::create_dir_all(&workspace_root).expect("the workspace");
    let destination =
        workspace::clone_destination(&workspace_root, &format!("{}/demo", scribe.login));
    let identity = GitIdentity {
        login: scribe.login.clone(),
        email: scribe.email.clone(),
    };
    let credential = GitCredential {
        askpass: Path::new(askpass_binary()),
        credentials: &credentials,
        agent: "scribe",
        login: &scribe.login,
    };
    workspace::prepare(
        &fj.url,
        &format!("{}/demo", scribe.login),
        &workspace_root,
        &destination,
        &identity,
        Some(&credential),
    )
    .expect("the agent's clone");
    git(&destination, &["checkout", "-q", "-b", "agent-work"]);
    fs::write(destination.join("agent-work.txt"), "the agent's work\n").expect("a file");
    git(&destination, &["add", "-A"]);
    git(&destination, &["commit", "-qm", "agent work"]);
    workspace::push(&destination, &credential).expect("the branch is pushed as the agent");

    credentials::record_form_secrets(
        &credentials,
        "scribe",
        Some("gw-live-model-key"),
        None,
        None,
    )
    .expect("the model key");

    let model_key = "gw-live-model-key";
    let script = |id: &str, name: &str, arguments: serde_json::Value| {
        StubReply::json(
            serde_json::json!({
                "model": "lab-chat-1",
                "choices": [{
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}
                        }]
                    }
                }]
            })
            .to_string(),
        )
    };
    let done = StubReply::json(
        serde_json::json!({
            "model": "lab-chat-1",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "done"}
            }]
        })
        .to_string(),
    );
    let repo_name = format!("{}/demo", scribe.login);
    let model = Stub::start(vec![
        script("c1", "forgejo_list_assigned_issues", serde_json::json!({})),
        script(
            "c2",
            "forgejo_read_issue_or_pull",
            serde_json::json!({"repository": repo_name, "number": 1}),
        ),
        script(
            "c3",
            "forgejo_comment",
            serde_json::json!({"repository": repo_name, "number": 1, "body": "I am on it."}),
        ),
        script("c4", "forgejo_list_my_pull_requests", serde_json::json!({})),
        script(
            "c5",
            "forgejo_open_pull_request",
            serde_json::json!({
                "repository": repo_name,
                "title": "Agent work for #1",
                "body": "Opened by the agent."
            }),
        ),
        script(
            "c6",
            "forgejo_update_pull_request",
            serde_json::json!({
                "repository": repo_name,
                "number": 2,
                "title": "Agent work for #1 (updated)",
                "body": "The agent updated its own request."
            }),
        ),
        done,
    ]);

    let run_dir = root.join("run-live-1");
    fs::create_dir_all(&run_dir).expect("the run directory");
    let instruction = root.join("instruction.md");
    fs::write(
        &instruction,
        "Work the issue, comment, then open and update the pull request.\n",
    )
    .expect("the instruction");
    RunPlan {
        run_id: "run-live-1".to_owned(),
        agent: "scribe".to_owned(),
        workspace: workspace_root.clone(),
        instruction,
        skills_dir: root.join("skills"),
        endpoint: model.url(),
        model: "lab-chat-1".to_owned(),
        credentials: credentials.clone(),
        max_turns: 12,
        steps: Vec::new(),
        timeout_secs: 90,
    }
    .write(&run_dir)
    .expect("the plan");
    fs::write(
        run_dir.join("forgejo.json"),
        serde_json::json!({
            "url": fj.url.clone(),
            "login": scribe.login.clone(),
            "repositories": [{"full_name": repo_name, "directory": "demo"}]
        })
        .to_string(),
    )
    .expect("the context");

    let status = Command::new(env!("CARGO_BIN_EXE_gewu-multiverse"))
        .arg("--run-agent")
        .arg(&run_dir)
        .status()
        .expect("the agent process");
    assert!(status.success(), "the live run exited {status:?}");
    let file = transcript::read(&run_dir).expect("the transcript");
    let actions: Vec<(String, String)> = file
        .records
        .iter()
        .filter_map(|record| match record {
            gewu_multiverse::transcript::Record::ForgejoResult {
                action: Some(action),
                url: Some(url),
                ..
            } => Some((action.clone(), url.clone())),
            _ => None,
        })
        .collect();
    assert!(
        actions.iter().any(|(action, _)| action == "commented"),
        "{actions:?}"
    );
    assert!(
        actions.iter().any(|(action, _)| action == "opened"),
        "{actions:?}"
    );
    assert!(
        actions.iter().any(|(action, _)| action == "updated"),
        "{actions:?}"
    );

    // The instance really has the comment and the updated request.
    let (status, text) = fj.api(
        &admin,
        "GET",
        &format!("/api/v1/repos/{repo_name}/issues/1/comments"),
        None,
    );
    assert_eq!(status, 200);
    assert!(text.contains("I am on it."), "{text}");
    let (status, text) = fj.api(
        &admin,
        "GET",
        &format!("/api/v1/repos/{repo_name}/pulls/2"),
        None,
    );
    assert_eq!(status, 200);
    assert!(text.contains("Agent work for #1 (updated)"), "{text}");
    let (status, text) = fj.api(
        agent_token.expose(),
        "GET",
        "/api/v1/repos/issues/search?state=open&assigned=true&type=issues&limit=10",
        None,
    );
    assert_eq!(status, 200);
    assert!(text.contains("Work for the agent"), "{text}");

    // No token reached the run's records, the workspace, or a model request. The
    // credentials file is the one place a token is allowed to live.
    let scan = |dir: &Path| {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = fs::read(&path) {
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(
                        !text.contains(agent_token.expose()),
                        "{} holds the agent token",
                        path.display()
                    );
                }
            }
        }
    };
    scan(&run_dir);
    scan(&workspace_root);
    for request in model.requests() {
        assert!(
            !request.body.contains(agent_token.expose()),
            "{}",
            request.body
        );
        assert!(!request.body.contains(model_key), "{}", request.body);
    }
}

/// A managed token made before the issue tools is renewed, and the token it replaces
/// stops answering the instance.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BIN=... cargo test --test forgejo -- --ignored"]
fn a_managed_token_is_renewed() {
    let fj = TestForgejo::start("rights");
    fj.create_user("root", ADMIN_PASSWORD, true);
    let admin = fj.access_token_for("root");
    let root = forgejo_support::scratch("rights-run");
    let credentials = root.join("credentials.toml");
    mode::write_private(
        &credentials,
        format!("forgejo_token = \"{admin}\"\nforgejo_admin_token = \"{admin}\"\n").as_bytes(),
    )
    .expect("the credentials file");

    let first = identity::manage(&fj.url, "scribe", &credentials).expect("the managed identity");
    let first_token =
        credentials::forgejo_agent_token(&credentials, "scribe").expect("the first token");
    let first_name = credentials::recorded_agent_token_name(&credentials, "scribe")
        .expect("the first token's name is recorded")
        .expect("the first token has a name");
    assert_eq!(first_name, "gewu-scribe");
    assert_eq!(
        credentials::recorded_agent_identity_rights(&credentials, "scribe")
            .expect("the marker")
            .as_deref(),
        Some(identity::TOOL_RIGHTS_VERSION)
    );

    // A record made before this change: the marker is old. `i` must make a new token
    // rather than reuse the one whose rights are unknown — and it must revoke the old
    // token on the server, not merely stop recording it (B1).
    credentials::record_agent_identity_rights(&credentials, "scribe", "before-issue-tools")
        .expect("the old marker");
    let second = identity::manage(&fj.url, "scribe", &credentials).expect("the renewal");
    let second_token =
        credentials::forgejo_agent_token(&credentials, "scribe").expect("the second token");
    let second_name = credentials::recorded_agent_token_name(&credentials, "scribe")
        .expect("the renewed token's name is recorded")
        .expect("the renewed token has a name");
    assert_ne!(first_name, second_name, "the renewal reused the token name");
    assert_eq!(first.login, second.login);
    assert_ne!(
        first_token.expose(),
        second_token.expose(),
        "the old managed token was reused after the marker was removed"
    );
    let client = ForgejoClient::new(&fj.url, second_token, DEFAULT_TIMEOUT).expect("a client");
    client
        .assigned_issues()
        .expect("the renewed token carries the issue right");
    // The superseded token now answers 401, and its name is gone from the server's
    // list: the renewal revoked it.
    let (old_status, _) = fj.api(first_token.expose(), "GET", "/api/v1/user", None);
    assert_eq!(
        old_status, 401,
        "the superseded managed token still answers the server"
    );
    let (list_status, list_body) = fj.api(
        &admin,
        "GET",
        "/api/v1/admin/users/gewu-scribe/tokens",
        None,
    );
    assert_eq!(list_status, 200, "the token list: {list_body}");
    let names: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&list_body)
        .expect("the token list is JSON")
        .into_iter()
        .filter_map(|token| {
            token
                .get("name")
                .and_then(|name| name.as_str())
                .map(str::to_owned)
        })
        .collect();
    assert!(
        !names.contains(&first_name),
        "the superseded token {first_name:?} is still listed: {names:?}"
    );
    assert!(
        names.contains(&second_name),
        "the renewed token {second_name:?} is not listed: {names:?}"
    );
    println!("renewal revoked {first_name:?}: the old token answers 401 and is no longer listed");
}
