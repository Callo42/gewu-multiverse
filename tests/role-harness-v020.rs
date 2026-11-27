//! Comprehensive verification for `gewu-multiverse` `v0.2.0`:
//!
//! Offline tests (run by default under `cargo test --locked`):
//! 1. `--configure` semantic roles (`--add-role`), wake modes, context views, `hydrate_cmd`,
//!    tiered permissions (`triage`/`review`/`maintain`), non-secret `--env`, `0600` `--secret-env-file`,
//!    and rejection of plaintext `--secret-env` arguments.
//! 2. Exact backward compatibility for `v0.1.43` / `matrixorg` `v0.7.0` configs.
//!
//! Live bundled Forgejo E2E tests (`#[ignore]`; run with
//! `GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64 cargo test --test role-harness-v020 -- --ignored --nocapture --test-threads=1`):
//! 3. Full 4-role autonomous GitHub loop (`pm` triage -> `coder` push + PR -> `reviewer` diff & review
//!    -> `maintainer` merge + create repo) with supervisor hard-blocks on self-review, self-merge,
//!    unapproved merge, and `REQUEST_CHANGES` merge.
//! 4. Sandboxed `hydrate_cmd` inside Landlock + role external secret redaction + Landlock denial of
//!    `credentials.toml` and sibling workspaces.
//! 5. Local Forgejo HMAC-SHA256 webhook wake for `event`-mode started roles + `desired = "stopped"`
//!    strictly 0-wake / 0-LLM-call iron law.
//! 6. TUI `RemoteDaemonEngine` auto-attach to live `--headless` daemon, safe detach (`shutdown()`
//!    leaves daemon & Forgejo alive), and explicit `stop_daemon()`.
//! 7. Exact `matrixorg` `v0.7.0` live `--headless --allow-idle` + `--control --stop-on-no-work` zero-regression proof.
//! 8. Real upgraded native harnesses (`dsh 0.2.2-rc.5`, `pi 1.6.1`, `codex 0.172.0`, `opencode 2.4.2`)
//!    via per-role resident `HarnessRuntime` under Landlock across two sequential ephemeral wakes
//!    (when `GEWU_V020_PROBE_ROOT` points to the unpacked official harness directories).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::credentials;
use gewu_multiverse::engine::settings::{ContextMode, RolePermissions, Settings, WakeMode};
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{Command as EngineCommand, Desired, EngineApi, ForgejoState, Provisioning};
use gewu_multiverse::forgejo::{DEFAULT_TIMEOUT, ForgejoClient};
use gewu_multiverse::forgejo_tools::{
    self, CREATE_ISSUE, CREATE_REPOSITORY, ForgejoTools, MERGE_PULL_REQUEST, OPEN_PULL_REQUEST,
    READ_PULL_DIFF, REVIEW_PULL_REQUEST,
};
use gewu_multiverse::harness::{InstallRecord, Program};
use gewu_multiverse::headless;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::{RunForgejo, RunForgejoRepository, process_alive};
use gewu_multiverse::test_stub::{Stub, StubReply};
use serde_json::{Value, json};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_gewu-multiverse")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-v020-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("scratch dir");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("0700");
        Self(root)
    }

    fn config(&self) -> PathBuf {
        self.0.join("config")
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    fn data(&self) -> PathBuf {
        self.0.join("data")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn base_cmd(scratch: &Scratch) -> Command {
    let mut cmd = Command::new(binary());
    cmd.arg("--config-dir")
        .arg(scratch.config())
        .arg("--state-dir")
        .arg(scratch.state())
        .arg("--data-dir")
        .arg(scratch.data())
        .stdin(Stdio::null());
    cmd
}

fn install_forgejo_from_env(data_root: &Path) {
    let source = std::env::var("GEWU_FORGEJO_BINARY")
        .expect("set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64");
    let source = PathBuf::from(source);
    assert!(source.is_file(), "Forgejo binary missing at {}", source.display());
    let dest = bundled_forgejo::binary_path(data_root);
    fs::create_dir_all(dest.parent().expect("parent")).expect("forgejo parent dir");
    fs::hard_link(&source, &dest)
        .or_else(|_| fs::copy(&source, &dest).map(|_| ()))
        .expect("copy forgejo");
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).expect("chmod forgejo");
    assert!(bundled_forgejo::is_installed(data_root));
}

fn chat_final_reply(text: &str) -> StubReply {
    StubReply::json(
        json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": text
                }
            }]
        })
        .to_string(),
    )
}

fn wait_until(label: &str, timeout: Duration, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {label}");
}

// ---------------------------------------------------------------------------
// 1. Offline `--configure` Layout A & Security Tests
// ---------------------------------------------------------------------------

#[test]
fn configure_creates_semantic_roles_permissions_context_env_and_0600_secrets() {
    let scratch = Scratch::new("configure-roles");
    let secret_literal = "sk-ext-top-secret-998877";

    // Plaintext `--secret-env` on CLI must be refused without echoing the secret.
    let refused = base_cmd(&scratch)
        .args([
            "--configure",
            "--secret-env",
            &format!("EXT_TOKEN={secret_literal}"),
        ])
        .output()
        .expect("run cli");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !stderr.contains(secret_literal),
        "refusal leaked secret: {stderr}"
    );

    let key_file = scratch.path().join("model.key");
    let ext_secret_file = scratch.path().join("ext.key");
    let inst_file = scratch.path().join("role.md");
    fs::write(&key_file, "sk-model-12345\n").expect("key file");
    fs::write(&ext_secret_file, format!("{secret_literal}\n")).expect("ext secret file");
    fs::write(&inst_file, "你是团队评审角色，负责审查所有开放 PR。\n").expect("inst file");

    let out = base_cmd(&scratch)
        .args([
            "--configure",
            "--accept-terms",
            "--base-url",
            "http://127.0.0.1:9999/v1",
            "--model",
            "synthetic-model",
            "--api-key-file",
            key_file.to_str().unwrap(),
            "--runtime-idle-secs",
            "180",
            "--add-role",
            "reviewer",
            "--program",
            "builtin",
            "--wake-mode",
            "event",
            "--wake-interval",
            "120",
            "--context-mode",
            "self",
            "--context-view",
            "open_pulls",
            "--context-repo",
            "team/workspace",
            "--context-label",
            "urgent",
            "--hydrate-cmd",
            "echo ok",
            "--allow-review",
            "--allow-maintain",
            "--write-bound",
            "12",
            "--env",
            "SERVICE_ENV=staging",
            "--secret-env-file",
            &format!("EXT_TOKEN={}", ext_secret_file.display()),
            "--instruction-file",
            inst_file.to_str().unwrap(),
        ])
        .output()
        .expect("configure reviewer");
    assert!(
        out.status.success(),
        "configure failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Inspect saved multiverse.toml: Layout A fields persisted, NO secrets written there.
    let toml_path = scratch.config().join("multiverse.toml");
    let toml_text = fs::read_to_string(&toml_path).expect("read toml");
    assert!(
        !toml_text.contains(secret_literal),
        "external secret leaked into multiverse.toml"
    );
    let settings = Settings::load(&toml_path).expect("load settings");
    assert_eq!(settings.default_harness.as_deref(), Some("dsh"));
    assert_eq!(settings.effective_runtime_idle_secs(), 180);
    let reviewer = settings.agent("reviewer").expect("reviewer role");
    assert_eq!(reviewer.effective_wake_mode(WakeMode::Hybrid), WakeMode::Event);
    assert_eq!(reviewer.wake_interval_secs, Some(120));
    let ctx = reviewer.effective_context(ContextMode::SelfOnly);
    assert_eq!(ctx.mode, ContextMode::SelfOnly);
    assert_eq!(ctx.views, vec!["open_pulls"]);
    assert_eq!(ctx.repos, vec!["team/workspace"]);
    assert_eq!(ctx.labels, vec!["urgent"]);
    assert_eq!(ctx.hydrate_cmd.as_deref(), Some("echo ok"));
    let perms = reviewer.effective_permissions();
    assert!(!perms.triage);
    assert!(perms.review);
    assert!(perms.maintain);
    assert_eq!(perms.effective_write_bound(), 12);
    assert_eq!(reviewer.env.get("SERVICE_ENV").map(String::as_str), Some("staging"));

    // Inspect 0600 credentials.toml: external secret stored and included in redaction set.
    let creds_path = scratch.config().join("credentials.toml");
    let mode = fs::metadata(&creds_path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let secrets = credentials::agent_env_secrets(&creds_path, "reviewer").expect("env secrets");
    assert_eq!(
        secrets.get("EXT_TOKEN").map(|s| s.expose()),
        Some(secret_literal)
    );
    assert!(
        credentials::all_secret_values(&creds_path)
            .iter()
            .any(|v| v.expose() == secret_literal)
    );

    // `--status --json` stays secret-free and reports semantic role cleanly.
    let status = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status json");
    assert!(status.status.success());
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(!stdout.contains(secret_literal));
    assert!(!stdout.contains("sk-model-12345"));
    let parsed: Value = serde_json::from_str(&stdout).expect("status JSON");
    assert_eq!(parsed["agents"][0]["name"], "reviewer");
    assert_eq!(parsed["agents"][0]["login"], "gewu-reviewer");
}

#[test]
fn legacy_v0143_matrixorg_config_inherits_builtin_periodic_none_and_least_privilege() {
    let scratch = Scratch::new("legacy-matrixorg");
    fs::create_dir_all(scratch.config().join("agents/agent-1")).expect("agent dir");
    fs::write(
        scratch.config().join("agents/agent-1/instruction.md"),
        "Check assigned issues and reply.\n",
    )
    .expect("instruction");
    let legacy_toml = r#"terms_acknowledged = true
terms_version = 2
next_agent = 2

[endpoint]
url = "http://127.0.0.1:19999/v1/chat/completions"
model = "legacy-chat"
max_turns = 24

[forgejo]
admin_login = "owner"

[[agents]]
name = "agent-1"
desired = "stopped"
harness = ""
"#;
    let toml_path = scratch.config().join("multiverse.toml");
    fs::write(&toml_path, legacy_toml).expect("write legacy toml");
    let creds = scratch.config().join("credentials.toml");
    credentials::CredentialsFile::prepare(&creds).expect("prepare creds");
    credentials::record_secret(&creds, None, "model_key", "sk-legacy-key").expect("key");

    let settings = Settings::load(&toml_path).expect("load legacy");
    assert_eq!(settings.default_harness, None);
    assert_eq!(settings.effective_wake_mode(), WakeMode::Periodic);
    assert_eq!(settings.effective_default_context(), ContextMode::None);
    let a1 = settings.agent("agent-1").expect("agent-1");
    assert_eq!(a1.harness(), Program::Builtin);
    assert_eq!(a1.effective_wake_mode(settings.effective_wake_mode()), WakeMode::Periodic);
    assert_eq!(a1.effective_wake_interval_secs(WakeMode::Periodic), 20);
    assert_eq!(a1.effective_context(ContextMode::None).mode, ContextMode::None);
    assert!(a1.effective_permissions().is_default());

    let status = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status");
    assert!(status.status.success());
    let parsed: Value = serde_json::from_slice(&status.stdout).expect("json");
    assert_eq!(parsed["ready_for_idle_headless"], true);
    assert_eq!(parsed["agents"][0]["program"], "builtin");
    assert_eq!(parsed["agents"][0]["desired"], "stopped");
}

// ---------------------------------------------------------------------------
// 2. Live Bundled Forgejo E2E Tests (`#[ignore]`)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "live bundled Forgejo E2E: set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64"]
fn live_v020_four_role_autonomous_github_loop_with_self_review_and_self_merge_guards() {
    let scratch = Scratch::new("four-role-loop");
    install_forgejo_from_env(&scratch.data());
    let layout = Layout::rooted_at(scratch.config())
        .expect("layout")
        .with_state_root(scratch.state())
        .expect("state");
    let mut options = EngineOptions::new(layout.clone(), scratch.data()).expect("options");
    options.product_binary = PathBuf::from(binary());
    let mut engine = engine::open_with(options).expect("open engine");
    engine.send(EngineCommand::AcknowledgeTerms).expect("terms");
    for role in ["pm", "coder", "reviewer", "maintainer"] {
        engine
            .send(EngineCommand::AddNamedAgent {
                name: role.to_owned(),
            })
            .expect("add role");
    }

    wait_until("Forgejo and 4 roles provisioned", Duration::from_secs(120), || {
        engine.tick();
        let b = engine.board();
        b.forgejo.state == ForgejoState::Running
            && b.forgejo.repositories >= 1
            && b.agents.len() == 4
            && b.agents.iter().all(|a| a.provisioning == Provisioning::Ready)
    });

    let forgejo_url = engine.board().forgejo.url.clone();
    let creds = layout.credentials_file();
    let admin_token = credentials::forgejo_admin_token(&creds)
        .expect("read admin token")
        .expect("admin token present");
    let make_tools = |role: &str, perms: RolePermissions| -> ForgejoTools {
        let token = credentials::forgejo_agent_token(&creds, role).expect("role token");
        let ws = layout.agent(role).expect("role paths").workspace().to_path_buf();
        let repo_dir = ws.join("workspace");
        if !repo_dir.exists() {
            let auth_url = forgejo_url.replacen(
                "http://",
                &format!("http://gewu-{role}:{}@", token.expose()),
                1,
            );
            let status = Command::new("git")
                .args([
                    "clone",
                    &format!("{auth_url}/team/workspace.git"),
                    repo_dir.to_str().unwrap(),
                ])
                .status()
                .expect("git clone for role");
            assert!(status.success(), "git clone failed for {role}");
        }
        let cfg = RunForgejo {
            url: forgejo_url.clone(),
            login: format!("gewu-{role}"),
            repositories: vec![RunForgejoRepository {
                full_name: "team/workspace".to_owned(),
                directory: "workspace".to_owned(),
            }],
        };
        ForgejoTools::new(cfg, ws, role, token)
            .with_permissions(perms)
            .with_org_creator_token(Some(admin_token.clone()))
    };

    // 1. PM (`triage = true`) creates an issue & assigns it to `gewu-coder`.
    let mut pm_tools = make_tools(
        "pm",
        RolePermissions {
            triage: true,
            ..Default::default()
        },
    );
    let create_res = pm_tools
        .execute(
            CREATE_ISSUE,
            &json!({
                "repository": "team/workspace",
                "title": "Add healthz endpoint",
                "body": "Please add healthz.txt",
                "assignees": ["gewu-coder"]
            })
            .to_string(),
        )
        .expect("pm create_issue should succeed");
    assert_eq!(create_res.action, Some("created_issue"));
    assert_eq!(pm_tools.writes(), 1);

    // Least-privilege coder CANNOT call create_issue or review_pull_request.
    let mut coder_tools = make_tools("coder", RolePermissions::default());
    let denied = coder_tools.execute(
        CREATE_ISSUE,
        &json!({
            "repository": "team/workspace",
            "title": "Unauthorized"
        })
        .to_string(),
    );
    assert!(denied.is_err(), "least-privilege coder should be refused create_issue");
    assert_eq!(coder_tools.writes(), 0);

    // 2. Coder pushes branch `feat/healthz`, opens PR via `forgejo_open_pull_request`.
    let coder_ws = layout.agent("coder").expect("coder").workspace().to_path_buf();
    let repo_dir = coder_ws.join("workspace");
    for args in [
        vec!["config", "user.name", "gewu-coder"],
        vec!["config", "user.email", "gewu-coder@agents.invalid"],
        vec!["checkout", "-b", "feat/healthz"],
    ] {
        assert!(Command::new("git").current_dir(&repo_dir).args(args).status().unwrap().success());
    }
    fs::write(repo_dir.join("healthz.txt"), "ok\n").expect("write healthz");
    for args in [
        vec!["add", "healthz.txt"],
        vec!["commit", "-m", "add healthz"],
        vec!["push", "-u", "origin", "feat/healthz"],
    ] {
        assert!(Command::new("git").current_dir(&repo_dir).args(args).status().unwrap().success());
    }
    let pr_res = coder_tools
        .execute(
            OPEN_PULL_REQUEST,
            &json!({
                "repository": "team/workspace",
                "title": "Add healthz.txt",
                "body": "Closes #1"
            })
            .to_string(),
        )
        .expect("coder open_pull_request should succeed");
    let pr_number = pr_res.number.expect("PR number returned");

    // 3. SECURITY NEGATIVE #1: Self-review MUST be hard-blocked even if coder has `review = true`.
    let mut coder_review_tools = make_tools(
        "coder",
        RolePermissions {
            review: true,
            maintain: true,
            ..Default::default()
        },
    );
    let self_rev = coder_review_tools.execute(
        REVIEW_PULL_REQUEST,
        &json!({
            "repository": "team/workspace",
            "number": pr_number,
            "event": "APPROVE",
            "body": "LGTM self"
        })
        .to_string(),
    );
    let self_rev_err = self_rev.expect_err("self-review MUST be blocked");
    assert!(
        self_rev_err.to_lowercase().contains("own pull request")
            || self_rev_err.to_lowercase().contains("cannot review")
            || self_rev_err.to_lowercase().contains("self"),
        "unexpected self-review message: {self_rev_err}"
    );

    // 4. SECURITY NEGATIVE #2: Maintainer cannot merge an unapproved PR.
    let mut maint_tools = make_tools(
        "maintainer",
        RolePermissions {
            maintain: true,
            ..Default::default()
        },
    );
    let unapproved_err = maint_tools
        .execute(
            MERGE_PULL_REQUEST,
            &json!({
                "repository": "team/workspace",
                "number": pr_number
            })
            .to_string(),
        )
        .expect_err("unapproved merge MUST be blocked");
    assert!(
        unapproved_err.to_lowercase().contains("approv"),
        "unexpected unapproved merge msg: {unapproved_err}"
    );

    // 5. Reviewer reads diff and submits REQUEST_CHANGES -> merge STILL blocked!
    let mut reviewer_tools = make_tools(
        "reviewer",
        RolePermissions {
            review: true,
            ..Default::default()
        },
    );
    let diff = reviewer_tools
        .execute(
            READ_PULL_DIFF,
            &json!({
                "repository": "team/workspace",
                "number": pr_number
            })
            .to_string(),
        )
        .expect("read_pull_diff");
    assert!(diff.text.contains("healthz.txt"), "diff: {}", diff.text);
    reviewer_tools
        .execute(
            REVIEW_PULL_REQUEST,
            &json!({
                "repository": "team/workspace",
                "number": pr_number,
                "event": "REQUEST_CHANGES",
                "body": "Please double-check"
            })
            .to_string(),
        )
        .expect("REQUEST_CHANGES review");

    let blocked_after_rc = maint_tools.execute(
        MERGE_PULL_REQUEST,
        &json!({
            "repository": "team/workspace",
            "number": pr_number
        })
        .to_string(),
    );
    assert!(blocked_after_rc.is_err(), "merge with REQUEST_CHANGES MUST be blocked");

    // 6. Reviewer submits APPROVE -> Self-merge by coder STILL blocked, Maintainer merge SUCCEEDS!
    reviewer_tools
        .execute(
            REVIEW_PULL_REQUEST,
            &json!({
                "repository": "team/workspace",
                "number": pr_number,
                "event": "APPROVE",
                "body": "Looks great"
            })
            .to_string(),
        )
        .expect("APPROVE review");

    let self_merge = coder_review_tools.execute(
        MERGE_PULL_REQUEST,
        &json!({
            "repository": "team/workspace",
            "number": pr_number
        })
        .to_string(),
    );
    assert!(self_merge.is_err(), "self-merge MUST be blocked even after external approval");

    let merged = maint_tools
        .execute(
            MERGE_PULL_REQUEST,
            &json!({
                "repository": "team/workspace",
                "number": pr_number
            })
            .to_string(),
        )
        .expect("maintainer merge after non-author approval should succeed");
    assert_eq!(merged.action, Some("merged"));

    // 7. Maintainer creates a new repo under `team/`.
    let new_repo = maint_tools
        .execute(
            CREATE_REPOSITORY,
            &json!({
                "name": "service-ops",
                "description": "Ops scripts"
            })
            .to_string(),
        )
        .expect("create_repository under team/ should succeed");
    assert_eq!(new_repo.action, Some("created_repository"));
    assert_eq!(
        forgejo_tools::allowed_tool_names(&RolePermissions::default()),
        forgejo_tools::TOOL_NAMES.to_vec()
    );

    engine.shutdown(Duration::from_secs(5));
}

#[test]
#[ignore = "live bundled Forgejo E2E: set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64"]
fn live_v020_sandboxed_hydrate_cmd_secret_redaction_and_landlock_isolation() {
    let scratch = Scratch::new("hydrate-landlock");
    install_forgejo_from_env(&scratch.data());

    let ext_secret = "sk-role-secret-canary-774411";
    let key_file = scratch.path().join("model.key");
    let ext_file = scratch.path().join("ext.key");
    let inst_file = scratch.path().join("ops.md");
    fs::write(&key_file, "sk-model-ops\n").expect("key");
    fs::write(&ext_file, format!("{ext_secret}\n")).expect("ext");
    fs::write(&inst_file, "检查快照后汇报没有待办工作。\n").expect("inst");

    let model = Stub::start(vec![
        chat_final_reply("目前没有需要处理的工作。"),
        chat_final_reply("目前没有需要处理的工作。"),
    ]);

    // Create sibling role with a secret marker inside its workspace.
    let sib_inst = scratch.path().join("sib.md");
    fs::write(&sib_inst, "sibling\n").expect("sib inst");
    assert!(
        base_cmd(&scratch)
            .args([
                "--configure",
                "--accept-terms",
                "--base-url",
                &model.url(),
                "--model",
                "synthetic-model",
                "--api-key-file",
                key_file.to_str().unwrap(),
                "--add-role",
                "sibling",
                "--program",
                "builtin",
                "--instruction-file",
                sib_inst.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success()
    );
    let sibling_secret_file = scratch.config().join("agents/sibling/workspace/secret.txt");
    fs::create_dir_all(sibling_secret_file.parent().unwrap()).expect("sib ws");
    fs::write(&sibling_secret_file, "SIBLING_PRIVATE_CANARY\n").expect("sib secret");

    // Configure `ops` with `hydrate_cmd` probing own workspace, env, secret redaction,
    // credentials.toml denial, and sibling workspace denial.
    let creds_path = scratch.config().join("credentials.toml");
    let hydrate_script = format!(
        "echo ENV=$SERVICE_ENV; echo LEAK=$EXT_API_KEY; \
         if cat '{}' >/dev/null 2>&1; then echo CREDS_READABLE; else echo CREDS_DENIED; fi; \
         if cat '{}' >/dev/null 2>&1; then echo SIBLING_READABLE; else echo SIBLING_DENIED; fi",
        creds_path.display(),
        sibling_secret_file.display()
    );
    let out = base_cmd(&scratch)
        .args([
            "--configure",
            "--add-role",
            "ops",
            "--program",
            "builtin",
            "--wake-mode",
            "periodic",
            "--context-mode",
            "self",
            "--hydrate-cmd",
            &hydrate_script,
            "--env",
            "SERVICE_ENV=staging",
            "--secret-env-file",
            &format!("EXT_API_KEY={}", ext_file.display()),
            "--instruction-file",
            inst_file.to_str().unwrap(),
            "--start-agent",
            "ops",
        ])
        .output()
        .expect("configure ops");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let layout = Layout::rooted_at(scratch.config())
        .expect("layout")
        .with_state_root(scratch.state())
        .expect("state");
    let mut options = EngineOptions::new(layout.clone(), scratch.data()).expect("options");
    options.product_binary = PathBuf::from(binary());
    let mut engine = engine::open_with(options).expect("open");

    wait_until("ops completes run #1", Duration::from_secs(120), || {
        engine.tick();
        let b = engine.board();
        b.agents
            .iter()
            .find(|a| a.name == "ops")
            .is_some_and(|a| a.runs_total >= 1 && !matches!(a.session, gewu_multiverse::engine_api::SessionState::Running { .. }))
    });

    let reqs = model.requests();
    assert!(!reqs.is_empty(), "model never called");
    let prompt_body = &reqs[0].body;
    assert!(
        prompt_body.contains("<environment-snapshot>"),
        "missing snapshot in prompt: {prompt_body}"
    );
    assert!(prompt_body.contains("ENV=staging"), "role env missing: {prompt_body}");
    assert!(
        !prompt_body.contains(ext_secret),
        "external secret leaked into prompt!"
    );
    assert!(
        prompt_body.contains("CREDS_DENIED"),
        "hydrate_cmd should be denied credentials.toml under Landlock: {prompt_body}"
    );
    assert!(
        prompt_body.contains("SIBLING_DENIED"),
        "hydrate_cmd should be denied sibling workspace under Landlock: {prompt_body}"
    );

    // Also verify run.log and transcript.jsonl contain zero plaintext external secret.
    let (log_path, transcript_path) = engine.session_files("ops").expect("session files");
    let log_text = fs::read_to_string(&log_path).unwrap_or_default();
    let tr_text = fs::read_to_string(&transcript_path).unwrap_or_default();
    assert!(!log_text.contains(ext_secret), "secret leaked into run.log");
    assert!(!tr_text.contains(ext_secret), "secret leaked into transcript.jsonl");

    engine.shutdown(Duration::from_secs(5));
}

#[test]
#[ignore = "live bundled Forgejo E2E: set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64"]
fn live_v020_event_webhook_wakes_started_role_while_stopped_role_stays_zero_calls() {
    let scratch = Scratch::new("webhook-iron-law");
    install_forgejo_from_env(&scratch.data());

    let key_file = scratch.path().join("model.key");
    let inst_file = scratch.path().join("coder.md");
    fs::write(&key_file, "sk-webhook-test\n").expect("key");
    fs::write(&inst_file, "检查指派给你的任务；若无则报告无工作。\n").expect("inst");

    let model = Stub::start(vec![
        chat_final_reply("目前没有指派给我的工作。"),
        chat_final_reply("目前没有指派给我的工作。"),
        chat_final_reply("目前没有指派给我的工作。"),
    ]);

    for (role, start) in [("coder", true), ("standby", false)] {
        let mut cmd = base_cmd(&scratch);
        cmd.args([
            "--configure",
            "--accept-terms",
            "--base-url",
            &model.url(),
            "--model",
            "synthetic-model",
            "--api-key-file",
            key_file.to_str().unwrap(),
            "--add-role",
            role,
            "--program",
            "builtin",
            "--wake-mode",
            "event",
            "--context-mode",
            "self",
            "--instruction-file",
            inst_file.to_str().unwrap(),
        ]);
        if start {
            cmd.args(["--start-agent", role]);
        }
        let out = cmd.output().expect("configure role");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    let layout = Layout::rooted_at(scratch.config())
        .expect("layout")
        .with_state_root(scratch.state())
        .expect("state");
    let mut options = EngineOptions::new(layout.clone(), scratch.data()).expect("options");
    options.product_binary = PathBuf::from(binary());
    let mut engine = engine::open_with(options).expect("open");

    // Wait for initial wake #1 of `coder` to complete and park silently in event mode.
    wait_until("coder finishes initial run #1", Duration::from_secs(120), || {
        engine.tick();
        let b = engine.board();
        b.agents.iter().all(|a| a.provisioning == Provisioning::Ready)
            && b.agents
                .iter()
                .find(|a| a.name == "coder")
                .is_some_and(|a| {
                    a.runs_total == 1
                        && !matches!(a.session, gewu_multiverse::engine_api::SessionState::Running { .. })
                })
    });

    // Ensure `coder` does NOT spin when parked in `event` mode (hold for 2 seconds).
    for _ in 0..20 {
        engine.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        engine.board().agents.iter().find(|a| a.name == "coder").unwrap().runs_total,
        1,
        "event-mode role should stay parked until a webhook arrives"
    );

    // Admin creates an issue assigned to BOTH `gewu-coder` (started) and `gewu-standby` (stopped).
    let forgejo_url = engine.board().forgejo.url.clone();
    let admin_token = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("read admin token")
        .expect("admin token present");
    let admin = ForgejoClient::new(&forgejo_url, admin_token, DEFAULT_TIMEOUT).expect("admin");
    admin
        .create_issue_full(
            "team",
            "workspace",
            "Webhook wake test",
            "Please pick this up",
            &["gewu-coder".to_owned(), "gewu-standby".to_owned()],
            &[],
        )
        .expect("create assigned issue");

    // Local Forgejo HMAC-SHA256 webhook wakes `coder` for run #2!
    wait_until("webhook wakes coder for run #2", Duration::from_secs(30), || {
        engine.tick();
        engine
            .board()
            .agents
            .iter()
            .find(|a| a.name == "coder")
            .is_some_and(|a| {
                a.runs_total >= 2
                    && !matches!(a.session, gewu_multiverse::engine_api::SessionState::Running { .. })
            })
    });

    // IRON LAW: `standby` (`desired = "stopped"`) MUST remain strictly at 0 runs!
    let board_after = engine.board();
    let standby = board_after
        .agents
        .iter()
        .find(|a| a.name == "standby")
        .expect("standby");
    assert_eq!(standby.desired, Desired::Stopped);
    assert_eq!(
        standby.runs_total, 0,
        "stopped role MUST NEVER be woken by webhooks!"
    );

    // Verify wake #2 prompt received the webhook wake reason + assigned issue snapshot.
    let reqs = model.requests();
    assert!(reqs.len() >= 2);
    assert!(
        reqs[1].body.contains("Webhook wake test"),
        "wake #2 snapshot missing assigned issue: {}",
        reqs[1].body
    );

    engine.shutdown(Duration::from_secs(5));
}

#[test]
#[ignore = "live bundled Forgejo E2E: set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64"]
fn live_v020_tui_remote_daemon_attach_detach_and_stop_daemon() {
    let scratch = Scratch::new("tui-attach");
    install_forgejo_from_env(&scratch.data());

    let key_file = scratch.path().join("model.key");
    let inst_file = scratch.path().join("a1.md");
    fs::write(&key_file, "sk-attach-key\n").expect("key");
    fs::write(&inst_file, "Idle standby role.\n").expect("inst");
    assert!(
        base_cmd(&scratch)
            .args([
                "--configure",
                "--accept-terms",
                "--base-url",
                "http://127.0.0.1:19999/v1",
                "--model",
                "synthetic-model",
                "--api-key-file",
                key_file.to_str().unwrap(),
                "--add-role",
                "coder",
                "--program",
                "builtin",
                "--instruction-file",
                inst_file.to_str().unwrap(),
                "--stop-all",
            ])
            .status()
            .unwrap()
            .success()
    );

    let mut daemon = base_cmd(&scratch)
        .args(["--headless", "--allow-idle"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn headless");
    let daemon_pid = daemon.id();

    let layout = Layout::rooted_at(scratch.config())
        .expect("layout")
        .with_state_root(scratch.state())
        .expect("state");

    let mut remote = None;
    wait_until("daemon ready for remote attach", Duration::from_secs(120), || {
        if let Some(mut r) = headless::try_attach_daemon(&layout, &scratch.data()) {
            let b = r.board();
            if b.forgejo.state == ForgejoState::Running {
                remote = Some(r);
                return true;
            }
        }
        false
    });

    let mut r = remote.expect("attached remote engine");
    assert!(r.is_attached());
    r.send(EngineCommand::AddNamedAgent {
        name: "qa".to_owned(),
    })
    .expect("add role via remote daemon socket");
    r.tick();
    assert!(r.board().agents.iter().any(|a| a.name == "qa"));

    // Detach (TUI `q`): daemon and Forgejo MUST continue running!
    r.shutdown(Duration::from_secs(1));
    drop(r);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        process_alive(daemon_pid),
        "detaching TUI MUST NOT stop the headless daemon!"
    );

    // Re-attach and explicitly stop the daemon (TUI `Q`).
    let mut r2 = headless::try_attach_daemon(&layout, &scratch.data())
        .expect("re-attach after detach");
    r2.stop_daemon().expect("stop_daemon");
    wait_until("daemon exits cleanly after stop_daemon", Duration::from_secs(20), || {
        daemon.try_wait().ok().flatten().is_some()
    });
    let status = daemon.wait().expect("wait");
    assert!(status.success(), "daemon did not exit 0: {status:?}");
}

#[test]
#[ignore = "live bundled Forgejo E2E: set GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64"]
fn live_v020_matrixorg_v070_exact_zero_regression_contract() {
    let scratch = Scratch::new("matrixorg-live");
    install_forgejo_from_env(&scratch.data());

    let model = Stub::start(vec![
        chat_final_reply("There are no open assigned issues right now."),
        chat_final_reply("There are no open assigned issues right now."),
    ]);

    // Write verbatim matrixorg v0.7.0 config (harness = "", desired = "stopped", zero v0.2.0 fields).
    fs::create_dir_all(scratch.config().join("agents/agent-1")).expect("agent dir");
    fs::write(
        scratch.config().join("agents/agent-1/instruction.md"),
        "Check assigned issues in Forgejo. If none, report no work.\n",
    )
    .expect("inst");
    let legacy_toml = format!(
        r#"terms_acknowledged = true
terms_version = 2
next_agent = 2

[endpoint]
url = "{}"
model = "matrixorg-chat"
max_turns = 24

[forgejo]
admin_login = "owner"

[[agents]]
name = "agent-1"
desired = "stopped"
harness = ""
"#,
        model.url()
    );
    fs::write(scratch.config().join("multiverse.toml"), legacy_toml).expect("toml");
    let creds = scratch.config().join("credentials.toml");
    credentials::CredentialsFile::prepare(&creds).expect("creds");
    credentials::record_secret(&creds, None, "model_key", "sk-matrixorg-test").expect("key");

    let mut daemon = base_cmd(&scratch)
        .args(["--headless", "--allow-idle"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn headless");

    // 1. Wait until healthy & idle_standby with strictly 0 LLM calls.
    wait_until("matrixorg daemon healthy in idle standby", Duration::from_secs(120), || {
        let out = base_cmd(&scratch).args(["--status", "--json"]).output().ok();
        let Some(out) = out.filter(|o| o.status.success()) else {
            return false;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["healthy"] == true && v["idle_standby"] == true
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        model.requests().len(),
        0,
        "matrixorg idle standby MUST make 0 model calls"
    );

    // 2. Wake via `--control --start-agent agent-1 --stop-on-no-work --json`.
    let ctrl = base_cmd(&scratch)
        .args([
            "--control",
            "--start-agent",
            "agent-1",
            "--stop-on-no-work",
            "--json",
        ])
        .output()
        .expect("control start");
    assert!(
        ctrl.status.success(),
        "control failed: {}",
        String::from_utf8_lossy(&ctrl.stderr)
    );
    let ctrl_json: Value = serde_json::from_slice(&ctrl.stdout).expect("control json");
    assert_eq!(ctrl_json["ok"], true);
    assert!(ctrl_json.get("board").is_none(), "legacy --control JSON must stay clean");

    // 3. Wait until `agent-1` runs once and auto-parks back to `stopped`.
    wait_until("matrixorg agent-1 auto-parks after NoWork", Duration::from_secs(60), || {
        let out = base_cmd(&scratch).args(["--status", "--json"]).output().ok();
        let Some(out) = out.filter(|o| o.status.success()) else {
            return false;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
            return false;
        };
        v["agents"][0]["runs_total"].as_u64().unwrap_or(0) >= 1
            && v["agents"][0]["desired"] == "stopped"
    });

    // Legacy `context = "none"` MUST NOT inject `<environment-snapshot>`.
    let reqs = model.requests();
    assert_eq!(reqs.len(), 1, "expected exactly 1 model call before auto-park");
    assert!(
        !reqs[0].body.contains("<environment-snapshot>"),
        "legacy matrixorg config must NOT inject <environment-snapshot>!"
    );

    // Clean shutdown.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    wait_until("matrixorg daemon exits on SIGTERM", Duration::from_secs(20), || {
        daemon.try_wait().ok().flatten().is_some()
    });
}

fn hardlink_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().expect("ft");
        if ft.is_dir() {
            hardlink_dir(&from, &to);
        } else if ft.is_symlink() {
            let link = fs::read_link(&from).expect("read_link");
            std::os::unix::fs::symlink(link, &to).expect("symlink");
        } else {
            fs::hard_link(&from, &to)
                .or_else(|_| fs::copy(&from, &to).map(|_| ()))
                .expect("copy/link");
        }
    }
}

fn stage_harness_from_probe(data_root: &Path, probe_root: &Path, program: Program) {
    let dest_root = data_root
        .join("harnesses")
        .join(program.id())
        .join(program.pinned_version());
    let (src_dir, entrypoint) = match program {
        Program::Dsh => (probe_root.join("dsh-test"), "node_modules/.bin/dsh"),
        Program::Pi => (probe_root.join("pi"), "pi/pi"),
        Program::Codex => (probe_root.join("codex-pkg"), "bin/codex"),
        Program::Opencode => (probe_root.join("opencode"), "package/bin/opencode"),
        Program::Builtin => return,
    };
    hardlink_dir(&src_dir, &dest_root);
    let record = InstallRecord {
        program: program.id().to_owned(),
        version: program.pinned_version().to_owned(),
        sha256: "e2e-probe".to_owned(),
        license: program.license().to_owned(),
        entrypoint: entrypoint.to_owned(),
        resources: None,
        installed_at: "2026-11-27T00:00:00Z".to_owned(),
    };
    let exe = dest_root.join(entrypoint);
    let _ = fs::set_permissions(&exe, fs::Permissions::from_mode(0o755));
    gewu_multiverse::mode::write_private(
        dest_root.join("install.json"),
        serde_json::to_string_pretty(&record).unwrap().as_bytes(),
    )
    .expect("install.json");
}

#[test]
#[ignore = "live 4-harness HarnessRuntime E2E: set GEWU_FORGEJO_BINARY and GEWU_V020_PROBE_ROOT"]
fn live_v020_all_four_harnesses_resident_runtime_ephemeral_sessions() {
    let probe_root = PathBuf::from(
        std::env::var("GEWU_V020_PROBE_ROOT")
            .expect("set GEWU_V020_PROBE_ROOT=/path/to/probe"),
    );

    // Run Pi (Chat Completions), DSH (Chat Completions), Codex (Responses), OpenCode (Chat Completions).
    for (program, protocol, replies) in [
        (
            Program::Pi,
            "chat-completions",
            vec![
                StubReply::sse(
                    "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_ONE_CANARY_PI: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
                StubReply::sse(
                    "data: {\"id\":\"cmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_TWO_OK_PI: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"cmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
            ],
        ),
        (
            Program::Dsh,
            "chat-completions",
            vec![
                StubReply::sse(
                    "data: {\"id\":\"dsh-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_ONE_CANARY_DSH: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"dsh-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
                StubReply::sse(
                    "data: {\"id\":\"dsh-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_TWO_OK_DSH: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"dsh-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
            ],
        ),
        (
            Program::Codex,
            "responses",
            vec![
                StubReply::sse(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"WAKE_ONE_CANARY_CODEX: no work\"}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"WAKE_ONE_CANARY_CODEX: no work\"}]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"WAKE_ONE_CANARY_CODEX: no work\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
                ),
                StubReply::sse(
                    "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\",\"status\":\"in_progress\"}}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_2\",\"output_index\":0,\"content_index\":0,\"delta\":\"WAKE_TWO_OK_CODEX: no work\"}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"WAKE_TWO_OK_CODEX: no work\"}]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"WAKE_TWO_OK_CODEX: no work\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
                ),
            ],
        ),
        (
            Program::Opencode,
            "chat-completions",
            vec![
                StubReply::sse(
                    "data: {\"id\":\"oc-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_ONE_CANARY_OPENCODE: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"oc-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
                StubReply::sse(
                    "data: {\"id\":\"oc-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"WAKE_TWO_OK_OPENCODE: no work\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"oc-2\",\"object\":\"chat.completion.chunk\",\"created\":2,\"model\":\"synthetic-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\ndata: [DONE]\n\n",
                ),
            ],
        ),
    ] {
        let scratch = Scratch::new(&format!("rt-{}", program.id()));
        install_forgejo_from_env(&scratch.data());
        stage_harness_from_probe(&scratch.data(), &probe_root, program);

        let model = Stub::start(replies);
        let base_url = model.url().trim_end_matches("/chat/completions").to_owned();
        let key_file = scratch.path().join("model.key");
        let inst_file = scratch.path().join("inst.md");
        fs::write(&key_file, "sk-runtime-e2e\n").expect("key");
        fs::write(&inst_file, "Reply briefly with status. No work needed.\n").expect("inst");

        let mut cfg = base_cmd(&scratch);
        cfg.args([
            "--configure",
            "--accept-terms",
            "--add-role",
            "worker",
            "--agent",
            "worker",
            "--program",
            program.id(),
            "--protocol",
            protocol,
            "--base-url",
            &base_url,
            "--model",
            "synthetic-model",
            "--api-key-file",
            key_file.to_str().unwrap(),
            "--wake-mode",
            "periodic",
            "--wake-interval",
            "2",
            "--context-mode",
            "self",
            "--instruction-file",
            inst_file.to_str().unwrap(),
        ]);
        if program == Program::Codex {
            cfg.args(["--codex-sandbox", "full-access"]);
        }
        let out = cfg.output().expect("configure harness");
        assert!(
            out.status.success(),
            "configure {} failed: {}",
            program.id(),
            String::from_utf8_lossy(&out.stderr)
        );

        let layout = Layout::rooted_at(scratch.config())
            .expect("layout")
            .with_state_root(scratch.state())
            .expect("state");
        let mut options = EngineOptions::new(layout.clone(), scratch.data()).expect("options");
        options.product_binary = PathBuf::from(binary());
        let mut engine = engine::open_with(options).expect("open");

        engine
            .send(EngineCommand::Start {
                name: "worker".to_owned(),
            })
            .expect("start worker");

        let pid_file = scratch.state().join("worker/runtime/runtime.pid");
        let mut first_pid: Option<u32> = None;

        // Wait for wake #1 to complete and record resident runtime pid.
        wait_until(
            &format!("{} completes wake #1", program.id()),
            Duration::from_secs(120),
            || {
                engine.tick();
                if first_pid.is_none()
                    && let Ok(text) = fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse::<u32>()
                {
                    first_pid = Some(pid);
                }
                engine
                    .board()
                    .agents
                    .iter()
                    .find(|a| a.name == "worker")
                    .is_some_and(|a| {
                        a.runs_total >= 1
                            && !matches!(a.session, gewu_multiverse::engine_api::SessionState::Running { .. })
                    })
            },
        );

        // Wait for wake #2 (2s periodic interval) to complete on the SAME resident runtime!
        wait_until(
            &format!("{} completes wake #2", program.id()),
            Duration::from_secs(90),
            || {
                engine.tick();
                engine
                    .board()
                    .agents
                    .iter()
                    .find(|a| a.name == "worker")
                    .is_some_and(|a| {
                        a.runs_total >= 2
                            && !matches!(a.session, gewu_multiverse::engine_api::SessionState::Running { .. })
                    })
            },
        );

        let second_pid = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        eprintln!(
            "[v0.2.0 E2E] harness={} first_runtime_pid={:?} second_runtime_pid={:?} requests={}",
            program.id(),
            first_pid,
            second_pid,
            model.requests().len()
        );
        assert!(
            first_pid.is_some() && first_pid == second_pid,
            "expected {} to reuse the same per-role resident runtime across wakes (first={:?}, second={:?})",
            program.id(),
            first_pid,
            second_pid
        );

        let reqs = model.requests();
        assert!(
            reqs.len() >= 2,
            "{}: expected >= 2 model requests, got {}",
            program.id(),
            reqs.len()
        );
        let canary = format!("WAKE_ONE_CANARY_{}", program.id().to_uppercase());
        assert!(
            !reqs[1].body.contains(&canary),
            "{}: wake #2 leaked wake #1 chat history! body={}",
            program.id(),
            reqs[1].body
        );

        // Stopping the role must stop its resident runtime cleanly!
        engine
            .send(EngineCommand::Stop {
                name: "worker".to_owned(),
            })
            .expect("stop worker");
        if let Some(pid) = first_pid {
            wait_until(
                &format!("{} resident runtime exits on role Stop", program.id()),
                Duration::from_secs(10),
                || !process_alive(pid),
            );
        }
        engine.shutdown(Duration::from_secs(5));
    }
}
