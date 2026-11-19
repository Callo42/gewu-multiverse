//! Non-interactive `--configure`, fail-fast `--headless` daemon startup, and
//! lock-free read-only `--status [--json]`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::run::process_alive;
use gewu_multiverse::test_stub::{Stub, StubReply};
use nix::fcntl::{Flock, FlockArg};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_gewu-multiverse")
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-headless-{tag}-{}-{nanos}",
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

#[test]
fn cli_refuses_plaintext_api_key_arguments_without_echoing_the_secret() {
    let secret = "sk-super-secret-value-99182";
    let eq_arg = format!("--api-key={secret}");
    let model_eq_arg = format!("--model-key={secret}");
    let unknown_eq_arg = format!("--secret={secret}");
    for args in [
        vec!["--configure", "--api-key", secret],
        vec!["--configure", "--model-key", secret],
        vec!["--configure", eq_arg.as_str()],
        vec!["--configure", "--program", "--api-key", secret],
        vec!["--configure", "--program", eq_arg.as_str()],
        vec!["--configure", "--remove-agent", model_eq_arg.as_str()],
        vec!["--configure", unknown_eq_arg.as_str()],
        vec!["--configure", secret],
    ] {
        let out = Command::new(binary())
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("binary runs");
        assert!(!out.status.success(), "unexpected success for {args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed for {args:?}:\nstdout={stdout}\nstderr={stderr}"
        );
    }
}

#[test]
fn headless_fails_fast_on_every_missing_precondition_with_repair_commands() {
    let scratch = Scratch::new("failfast");

    // 1. Fresh empty installation -> fails fast on unacknowledged terms with repair command.
    let out = base_cmd(&scratch)
        .arg("--headless")
        .output()
        .expect("runs");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("first-run terms have not been acknowledged")
            && stderr.contains("--accept-terms"),
        "{stderr}"
    );

    // 2. Terms accepted (via `--headless --accept-terms`), but no started agent -> fails fast with repair command.
    let out = base_cmd(&scratch)
        .args(["--headless", "--accept-terms"])
        .output()
        .expect("runs");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no agent is set to start") && stderr.contains("--start-agent"),
        "{stderr}"
    );

    // 3. Configure an agent without an instruction and attempt `--start-agent`:
    //    refused at `--configure` WITHOUT leaving `desired = "started"` persisted!
    let key_file = scratch.path().join("model.key");
    fs::write(&key_file, "sk-test-secret-001\n").expect("key file");
    let out = base_cmd(&scratch)
        .args([
            "--configure",
            "--agents",
            "1",
            "--agent",
            "agent-1",
            "--program",
            "builtin",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "demo-model",
            "--api-key-file",
        ])
        .arg(&key_file)
        .args(["--start-agent", "agent-1"])
        .output()
        .expect("configure runs");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("instruction is empty") && stderr.contains("--instruction-file"),
        "expected empty-instruction refusal with repair command: {stderr}"
    );
    let saved_toml =
        fs::read_to_string(scratch.config().join("multiverse.toml")).expect("multiverse.toml");
    assert!(
        saved_toml.contains(r#"desired = "stopped""#),
        "failed --start-agent must not persist desired=started:\n{saved_toml}"
    );

    // 4. Repair by supplying `--instruction-file` + `--start-agent` AND rerun identically to prove idempotency (F-2)!
    let inst_file = scratch.path().join("inst.md");
    fs::write(&inst_file, "Handle assigned issues.\n").expect("inst");
    for attempt in 1..=2 {
        let ok = base_cmd(&scratch)
            .args(["--configure", "--agent", "agent-1", "--instruction-file"])
            .arg(&inst_file)
            .args(["--start-agent", "agent-1"])
            .output()
            .expect("repair/rerun");
        assert!(
            ok.status.success(),
            "attempt {attempt} failed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    // 5. Missing model key at `--headless` startup -> fails fast with `--api-key-file` repair command (F-4).
    fs::remove_file(scratch.config().join("credentials.toml")).expect("drop creds");
    let no_key = base_cmd(&scratch)
        .arg("--headless")
        .output()
        .expect("headless no key");
    assert!(!no_key.status.success());
    let no_key_err = String::from_utf8_lossy(&no_key.stderr);
    assert!(
        no_key_err.contains("model key") && no_key_err.contains("--api-key-file"),
        "expected missing-key repair command: {no_key_err}"
    );

    // 6. Uninstalled native program (`codex`) on a started agent -> fails fast with `--install-harness codex` repair command (F-4).
    let restore_key = base_cmd(&scratch)
        .args([
            "--configure",
            "--agent",
            "agent-1",
            "--program",
            "codex",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "gpt-5-codex",
            "--api-key-file",
        ])
        .arg(&key_file)
        .output()
        .expect("configure codex");
    assert!(
        restore_key.status.success(),
        "{}",
        String::from_utf8_lossy(&restore_key.stderr)
    );
    let no_prog = base_cmd(&scratch)
        .arg("--headless")
        .output()
        .expect("headless uninstalled codex");
    assert!(!no_prog.status.success());
    let no_prog_err = String::from_utf8_lossy(&no_prog.stderr);
    assert!(
        no_prog_err.contains("not installed") && no_prog_err.contains("--install-harness codex"),
        "expected install-harness repair command: {no_prog_err}"
    );

    // 7. Initial Forgejo startup failure (tampered `GEWU_FORGEJO_BINARY`) -> fails fast with `--install-forgejo` repair command (F-4).
    let switch_back = base_cmd(&scratch)
        .args([
            "--configure",
            "--agent",
            "agent-1",
            "--program",
            "builtin",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "demo-model",
            "--api-key-file",
        ])
        .arg(&key_file)
        .args(["--start-agent", "agent-1"])
        .output()
        .expect("switch back to builtin");
    assert!(
        switch_back.status.success(),
        "{}",
        String::from_utf8_lossy(&switch_back.stderr)
    );
    let bad_forgejo = scratch.path().join("bad-forgejo");
    fs::write(&bad_forgejo, b"not-a-valid-forgejo\n").expect("bad forgejo");
    let bad_fj = base_cmd(&scratch)
        .arg("--headless")
        .env("GEWU_FORGEJO_BINARY", &bad_forgejo)
        .output()
        .expect("headless bad forgejo");
    assert!(!bad_fj.status.success());
    let bad_fj_err = String::from_utf8_lossy(&bad_fj.stderr);
    assert!(
        bad_fj_err.contains("bundled Forgejo failed to start")
            && bad_fj_err.contains("--install-forgejo"),
        "expected forgejo repair command: {bad_fj_err}"
    );
}

#[test]
fn configure_supports_oneshot_codex_offline_remove_agent_and_url_normalization() {
    let scratch = Scratch::new("codex-remove-norm");
    let key_file = scratch.path().join("key.txt");
    fs::write(&key_file, "sk-codex-key-123\n").expect("key");
    let inst_file = scratch.path().join("inst.md");
    fs::write(&inst_file, "Review pull requests.\n").expect("inst");

    // N-2: Selecting a native program (`--program codex`) without connection inputs when
    // no default connection exists refuses cleanly instead of writing an empty provider.
    let bare_codex = base_cmd(&scratch)
        .args(["--configure", "--agents", "2", "--agent", "agent-1", "--program", "codex"])
        .output()
        .expect("bare codex");
    assert!(!bare_codex.status.success());
    let toml_after_bare =
        fs::read_to_string(scratch.config().join("multiverse.toml")).expect("toml");
    assert!(
        !toml_after_bare.contains(r#"harness = "codex""#),
        "staged native program without connection must not leave empty provider:\n{toml_after_bare}"
    );

    // F-3: Incomplete one-shot Codex configuration (missing `--model`) fails atomically
    // without leaving a partial `harness = "codex"` entry behind.
    let bad_codex = base_cmd(&scratch)
        .args([
            "--configure",
            "--agent",
            "agent-1",
            "--program",
            "codex",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--api-key-file",
        ])
        .arg(&key_file)
        .output()
        .expect("bad codex");
    assert!(!bad_codex.status.success());
    let toml_after_bad =
        fs::read_to_string(scratch.config().join("multiverse.toml")).expect("toml");
    assert!(
        !toml_after_bad.contains(r#"harness = "codex""#),
        "failed model config must not leave partial codex harness:\n{toml_after_bad}"
    );

    // F-3: Valid one-shot Codex setup WITHOUT `--protocol responses` succeeds and stores `protocol = "responses"`.
    let ok_codex = base_cmd(&scratch)
        .args([
            "--configure",
            "--agent",
            "agent-1",
            "--program",
            "codex",
            "--codex-sandbox",
            "full-access",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "gpt-5-codex",
            "--api-key-file",
        ])
        .arg(&key_file)
        .arg("--instruction-file")
        .arg(&inst_file)
        .output()
        .expect("ok codex");
    assert!(
        ok_codex.status.success(),
        "{}",
        String::from_utf8_lossy(&ok_codex.stderr)
    );
    let toml_after_ok =
        fs::read_to_string(scratch.config().join("multiverse.toml")).expect("toml");
    assert!(
        toml_after_ok.contains(r#"harness = "codex""#)
            && toml_after_ok.contains(r#"protocol = "responses""#),
        "{toml_after_ok}"
    );

    // F-1 (offline unprovisioned): `--remove-agent agent-2` removes `agent-2` immediately from settings and `--status`.
    let rm = base_cmd(&scratch)
        .args(["--configure", "--remove-agent", "agent-2"])
        .output()
        .expect("remove agent-2");
    assert!(
        rm.status.success(),
        "{}",
        String::from_utf8_lossy(&rm.stderr)
    );
    let st = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status");
    let st_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&st.stdout)).expect("json");
    let agents = st_val["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1, "agent-2 should be removed: {st_val}");
    assert_eq!(agents[0]["name"], serde_json::json!("agent-1"));

    // F-1 (degraded provisioned): when a Forgejo instance record exists (`app.ini`) and Forgejo cannot start,
    // `--remove-agent` exits non-zero, stops the agent, and writes durable `removal-requested`.
    let fj_conf = bundled_forgejo::InstancePaths::under(&scratch.data());
    fs::create_dir_all(fj_conf.app_ini.parent().expect("parent")).expect("custom/conf");
    fs::write(&fj_conf.app_ini, "HTTP_PORT = 39999\n").expect("app.ini");
    let bad_forgejo = scratch.path().join("bad-forgejo");
    fs::write(&bad_forgejo, b"invalid\n").expect("bad forgejo");
    let rm_degraded = base_cmd(&scratch)
        .args(["--configure", "--remove-agent", "agent-1"])
        .env("GEWU_FORGEJO_BINARY", &bad_forgejo)
        .output()
        .expect("degraded remove");
    assert!(
        !rm_degraded.status.success(),
        "must not claim removal succeeded when Forgejo cannot revoke login"
    );
    assert!(
        scratch.state().join("agent-1/removal-requested").is_file(),
        "durable removal-requested marker must be persisted"
    );

    // N-5: Reconfiguring an agent that has `removal-requested` set is refused cleanly.
    let reconf_blocked = base_cmd(&scratch)
        .args([
            "--configure",
            "--agent",
            "agent-1",
            "--program",
            "pi",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "m-ignored",
            "--api-key-file",
        ])
        .arg(&key_file)
        .output()
        .expect("reconfigure blocked");
    assert!(!reconf_blocked.status.success());
    let reconf_err = String::from_utf8_lossy(&reconf_blocked.stderr);
    assert!(
        reconf_err.contains("queued for removal") && reconf_err.contains("--remove-agent agent-1"),
        "expected queued-for-removal refusal: {reconf_err}"
    );

    // N-3: Running `--headless --accept-terms` when no agent is started and `removal-requested`
    // is pending surfaces the pending removal and its `--remove-agent` command.
    let hl_pending = base_cmd(&scratch)
        .args(["--headless", "--accept-terms"])
        .output()
        .expect("headless with pending removal");
    assert!(!hl_pending.status.success());
    let hl_pending_err = String::from_utf8_lossy(&hl_pending.stderr);
    assert!(
        hl_pending_err.contains("pending removal for `agent-1`")
            && hl_pending_err.contains("--remove-agent agent-1"),
        "expected pending-removal hint: {hl_pending_err}"
    );

    // F-5 & F-6: Full `/v1/chat/completions` URL is normalized (never double-suffixed), and
    // `--test-model --agent no-such-agent` fails instead of silently testing the default endpoint.
    let stub = Stub::start(vec![StubReply::json(
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    )]);
    let norm = base_cmd(&scratch)
        .args([
            "--configure",
            "--base-url",
            &stub.url(),
            "--model",
            "m-norm",
            "--api-key-file",
        ])
        .arg(&key_file)
        .arg("--test-model")
        .output()
        .expect("test normalized full URL");
    assert!(
        norm.status.success(),
        "{}",
        String::from_utf8_lossy(&norm.stderr)
    );
    let reqs = stub.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].path, "/v1/chat/completions",
        "SetConnection double-suffixed full URL: {:?}",
        reqs[0].path
    );

    let missing_agent_test = base_cmd(&scratch)
        .args(["--configure", "--agent", "no-such-agent", "--test-model"])
        .output()
        .expect("nonexistent agent test-model");
    assert!(!missing_agent_test.status.success());
    assert!(
        String::from_utf8_lossy(&missing_agent_test.stderr)
            .contains("there is no agent named `no-such-agent`")
    );
}

#[test]
fn configure_bootstraps_without_tty_and_status_never_exposes_secrets_or_blocks_on_lock() {
    let scratch = Scratch::new("configure-status");
    let secret = "sk-redact-me-77889900";
    let inst_file = scratch.path().join("agent-1.md");
    fs::write(&inst_file, "Check assigned issues in team/workspace.\n").expect("instruction");

    let stub = Stub::start(vec![StubReply::json(
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    )]);

    // Bootstrap everything in one non-interactive command reading the secret from stdin,
    // including a live `--test-model` round-trip against the local stub.
    let mut child = Command::new(binary())
        .arg("--configure")
        .arg("--config-dir")
        .arg(scratch.config())
        .arg("--state-dir")
        .arg(scratch.state())
        .arg("--data-dir")
        .arg(scratch.data())
        .args([
            "--accept-terms",
            "--agents",
            "1",
            "--agent",
            "agent-1",
            "--program",
            "builtin",
            "--base-url",
            &format!("http://{}/v1", stub.addr()),
            "--model",
            "stub-chat",
            "--api-key-stdin",
            "--instruction-file",
        ])
        .arg(&inst_file)
        .args(["--start-agent", "agent-1", "--test-model"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("configure spawns");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{secret}\n").as_bytes())
        .expect("write secret");
    let out = child.wait_with_output().expect("configure finishes");
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // `credentials.toml` must be mode 0600.
    let creds_meta = fs::metadata(scratch.config().join("credentials.toml")).expect("creds");
    assert_eq!(creds_meta.permissions().mode() & 0o777, 0o600);

    // `--status` and `--status --json` report ready_for_headless=true and never print the secret.
    let status_txt = base_cmd(&scratch)
        .arg("--status")
        .output()
        .expect("status");
    assert!(status_txt.status.success());
    let txt = String::from_utf8_lossy(&status_txt.stdout);
    assert!(txt.contains("ready_for_headless: true"), "{txt}");
    assert!(!txt.contains(secret), "secret leaked in --status: {txt}");

    let status_json = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status --json");
    assert!(status_json.status.success());
    let raw_json = String::from_utf8_lossy(&status_json.stdout);
    assert!(
        !raw_json.contains(secret),
        "secret leaked in --status --json: {raw_json}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw_json).expect("valid JSON");
    assert_eq!(parsed["ready_for_headless"], serde_json::json!(true));
    assert_eq!(parsed["terms_acknowledged"], serde_json::json!(true));
    assert_eq!(parsed["engine_running"], serde_json::json!(false));
    assert_eq!(parsed["agents"][0]["name"], serde_json::json!("agent-1"));
    assert_eq!(parsed["agents"][0]["desired"], serde_json::json!("started"));
    assert_eq!(parsed["agents"][0]["key_present"], serde_json::json!(true));

    // Simulate a running daemon holding `engine.lock`:
    // - `--status --json` must return immediately with `engine_running: true` without blocking;
    // - `--configure` must refuse immediately instead of corrupting state.
    let lock_path = bundled_forgejo::lock_path(&scratch.data());
    fs::create_dir_all(lock_path.parent().expect("parent")).expect("dir");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open lock");
    let held = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock).expect("lock held");

    let start = Instant::now();
    let locked_status = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status under lock");
    assert!(locked_status.status.success());
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "--status blocked on engine.lock"
    );
    let locked_parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&locked_status.stdout)).expect("json");
    assert_eq!(locked_parsed["engine_running"], serde_json::json!(true));

    let refused_cfg = base_cmd(&scratch)
        .args(["--configure", "--add-agent"])
        .output()
        .expect("configure under lock");
    assert!(!refused_cfg.status.success());
    assert!(
        String::from_utf8_lossy(&refused_cfg.stderr).contains("running"),
        "{}",
        String::from_utf8_lossy(&refused_cfg.stderr)
    );
    drop(held);
}

#[test]
#[ignore = "live Forgejo headless daemon E2E: run with GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64 cargo test --test headless-daemon -- --ignored --nocapture"]
fn live_headless_daemon_runs_forgejo_and_agent_without_tty_and_stops_cleanly_on_sigterm() {
    let forgejo_bin = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("set GEWU_FORGEJO_BINARY to the pinned forgejo-17.0.0-linux-amd64 binary")
    }));
    assert!(forgejo_bin.is_file());

    let scratch = Scratch::new("live-daemon");
    let key_file = scratch.path().join("model.key");
    fs::write(&key_file, "sk-live-headless-test-key\n").expect("key");
    let inst_file = scratch.path().join("instruction.md");
    fs::write(
        &inst_file,
        "Inspect assigned issues. If none are assigned, report that there is no assigned work.\n",
    )
    .expect("instruction");

    // Loopback stub model answering agent-1's session turns cleanly with "No assigned work."
    let replies: Vec<StubReply> = (0..12)
        .map(|_| {
            StubReply::json(
                r#"{"choices":[{"message":{"role":"assistant","content":"No assigned issues found; nothing to do."}}]}"#,
            )
        })
        .collect();
    let stub = Stub::start(replies);

    let cfg_out = base_cmd(&scratch)
        .args([
            "--configure",
            "--accept-terms",
            "--agents",
            "2",
            "--agent",
            "agent-1",
            "--program",
            "builtin",
            "--base-url",
            &format!("http://{}/v1", stub.addr()),
            "--model",
            "stub-headless",
            "--api-key-file",
        ])
        .arg(&key_file)
        .arg("--instruction-file")
        .arg(&inst_file)
        .args(["--start-agent", "agent-1"])
        .output()
        .expect("configure");
    assert!(
        cfg_out.status.success(),
        "{}",
        String::from_utf8_lossy(&cfg_out.stderr)
    );

    // Spawn `--headless` detached from any controlling terminal (`setsid` via process_group + null stdin).
    use std::os::unix::process::CommandExt;
    let mut daemon = base_cmd(&scratch)
        .arg("--headless")
        .env("GEWU_FORGEJO_BINARY", &forgejo_bin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("headless daemon starts");
    let daemon_pid = daemon.id();

    // Poll `--status --json` from outside until `healthy == true` and `runs_total >= 1`.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut forgejo_pid: Option<u32> = None;
    let forgejo_url = loop {
        if let Some(status) = daemon.try_wait().expect("try_wait") {
            let out = daemon.wait_with_output().expect("output");
            panic!(
                "headless daemon exited early ({status}):\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let st = base_cmd(&scratch)
            .args(["--status", "--json"])
            .output()
            .expect("status --json");
        if st.status.success()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&st.stdout))
            && v["healthy"] == serde_json::json!(true)
        {
            let url = v["forgejo"]["url"].as_str().unwrap_or_default().to_owned();
            let pid_path = bundled_forgejo::InstancePaths::under(&scratch.data())
                .root
                .join(bundled_forgejo::PID_FILE);
            if let Ok(t) = fs::read_to_string(&pid_path) {
                forgejo_pid = t.trim().parse::<u32>().ok();
            }
            if v["agents"][0]["runs_total"].as_u64().unwrap_or(0) >= 1
                && v["agents"][1]["provisioning"] == serde_json::json!("ready")
            {
                break url;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for headless Forgejo + first agent run"
        );
        std::thread::sleep(Duration::from_millis(500));
    };

    assert!(
        forgejo_url.starts_with("http://127.0.0.1:"),
        "unexpected forgejo_url: {forgejo_url}"
    );
    let fpid = forgejo_pid.expect("forgejo.pid recorded while running");
    assert!(process_alive(fpid), "Forgejo process {fpid} is alive");

    // Send SIGTERM to the headless daemon and verify clean exit 0 and zero orphan processes!
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("SIGTERM sent");
    let out = daemon.wait_with_output().expect("daemon exits");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "daemon did not exit 0 on SIGTERM ({:?}):\n{stderr}",
        out.status
    );
    assert!(
        !stderr.contains("sk-live-headless-test-key"),
        "secret leaked in daemon logs"
    );
    assert!(
        !process_alive(fpid),
        "Forgejo child pid {fpid} survived SIGTERM shutdown"
    );
    assert!(
        !process_alive(daemon_pid),
        "daemon pid {daemon_pid} survived"
    );

    let pre_rm_status = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("pre-remove status");
    let pre_rm_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&pre_rm_status.stdout)).expect("json");
    let runs_before_rm = pre_rm_val["agents"][0]["runs_total"]
        .as_u64()
        .expect("runs_total");
    let reqs_before_rm = stub.requests().len();

    // Live F-1 & N-1 proof: `agent-2` was provisioned on real Forgejo during the daemon run.
    // Running `--configure --remove-agent agent-2` starts loopback Forgejo, revokes
    // `gewu-agent-2`'s token and deactivates its account, removes `agent-2` from
    // `multiverse.toml` and `credentials.toml`, NEVER starts `agent-1` or makes any model
    // request (`runs_total` and `stub.requests()` stay identical), and stops Forgejo cleanly.
    let rm_live = base_cmd(&scratch)
        .args(["--configure", "--remove-agent", "agent-2"])
        .env("GEWU_FORGEJO_BINARY", &forgejo_bin)
        .output()
        .expect("live remove-agent");
    assert!(
        rm_live.status.success(),
        "live --remove-agent failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&rm_live.stdout),
        String::from_utf8_lossy(&rm_live.stderr)
    );
    let post_rm_status = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("post-remove status");
    let post_rm_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&post_rm_status.stdout)).expect("json");
    let post_agents = post_rm_val["agents"].as_array().expect("agents");
    assert_eq!(post_agents.len(), 1, "agent-2 remained after live removal: {post_rm_val}");
    assert_eq!(post_agents[0]["name"], serde_json::json!("agent-1"));
    assert_eq!(
        post_agents[0]["runs_total"].as_u64(),
        Some(runs_before_rm),
        "--configure --remove-agent must not schedule runs for other started agents"
    );
    assert_eq!(
        stub.requests().len(),
        reqs_before_rm,
        "--configure --remove-agent must not send model requests for other started agents"
    );
    let creds_path = scratch.config().join("credentials.toml");
    assert!(
        gewu_multiverse::credentials::forgejo_agent_token_optional(&creds_path, "agent-2")
            .expect("read creds")
            .is_none(),
        "agent-2 Forgejo token remained in credentials.toml"
    );
    assert!(
        gewu_multiverse::credentials::agent_model_key(&creds_path, "agent-2")
            .expect("read creds")
            .is_none(),
        "agent-2 model key remained in credentials.toml"
    );
    let rm_pid_path = bundled_forgejo::InstancePaths::under(&scratch.data())
        .root
        .join(bundled_forgejo::PID_FILE);
    if let Ok(t) = fs::read_to_string(&rm_pid_path)
        && let Ok(p) = t.trim().parse::<u32>()
    {
        assert!(
            !process_alive(p),
            "Forgejo child pid {p} survived --configure --remove-agent shutdown"
        );
    }
}
