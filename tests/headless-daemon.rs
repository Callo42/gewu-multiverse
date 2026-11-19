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

static LIVE_FORGEJO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct DaemonGuard(Option< std::process::Child >);

impl DaemonGuard {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn id(&self) -> u32 {
        self.0.as_ref().expect("child").id()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.as_mut().expect("child").try_wait()
    }

    fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        self.0.take().expect("child").wait_with_output()
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
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
    let _serial = LIVE_FORGEJO_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let mut daemon = DaemonGuard::new(
        base_cmd(&scratch)
            .arg("--headless")
            .env("GEWU_FORGEJO_BINARY", &forgejo_bin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("headless daemon starts"),
    );
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

#[test]
fn idle_headless_and_control_validate_preconditions_socket_permissions_and_offline_errors() {
    let scratch = Scratch::new("idle-control-guards");
    let secret = "sk-control-never-echo-998877";
    let key_file = scratch.path().join("model.key");
    fs::write(&key_file, format!("{secret}\n")).expect("key file");
    let inst_file = scratch.path().join("agent-1.md");
    fs::write(&inst_file, "Handle event-driven tasks.\n").expect("inst file");

    // 1. `--headless --allow-idle` with 0 configured agents still fails fast.
    let zero_agents = base_cmd(&scratch)
        .args(["--headless", "--accept-terms", "--allow-idle"])
        .output()
        .expect("zero agents");
    assert!(!zero_agents.status.success());
    let zero_err = String::from_utf8_lossy(&zero_agents.stderr);
    assert!(
        zero_err.contains("no agents are configured"),
        "expected empty-agents error: {zero_err}"
    );

    // 2. Configure agent-1 started, then `--configure --stop-all` parks it to stopped.
    let cfg = base_cmd(&scratch)
        .args([
            "--configure",
            "--accept-terms",
            "--agents",
            "1",
            "--agent",
            "agent-1",
            "--program",
            "builtin",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--model",
            "stub-model",
            "--api-key-file",
        ])
        .arg(&key_file)
        .arg("--instruction-file")
        .arg(&inst_file)
        .args(["--start-agent", "agent-1"])
        .output()
        .expect("configure");
    assert!(
        cfg.status.success(),
        "{}",
        String::from_utf8_lossy(&cfg.stderr)
    );

    let stop_all = base_cmd(&scratch)
        .args(["--configure", "--stop-all"])
        .output()
        .expect("stop-all");
    assert!(
        stop_all.status.success(),
        "{}",
        String::from_utf8_lossy(&stop_all.stderr)
    );

    let st_out = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status");
    assert!(st_out.status.success());
    let st: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&st_out.stdout)).expect("json");
    assert_eq!(st["ready_for_headless"], serde_json::json!(false));
    assert_eq!(st["ready_for_idle_headless"], serde_json::json!(true));
    assert_eq!(st["agents"][0]["desired"], serde_json::json!("stopped"));

    // 3. Bare `--headless` (without `--allow-idle`) still fails fast when 0 agents are started.
    let bare_hl = base_cmd(&scratch)
        .arg("--headless")
        .output()
        .expect("bare headless");
    assert!(!bare_hl.status.success());
    let bare_err = String::from_utf8_lossy(&bare_hl.stderr);
    assert!(
        bare_err.contains("no agent is set to start") && bare_err.contains("--allow-idle"),
        "expected fail-fast with --allow-idle hint: {bare_err}"
    );

    // 4. `--control` when no daemon is running fails immediately without hanging.
    let started_at = Instant::now();
    let offline_ctrl = base_cmd(&scratch)
        .args(["--control", "--start-agent", "agent-1"])
        .output()
        .expect("offline --control");
    assert!(!offline_ctrl.status.success());
    assert!(
        started_at.elapsed() < Duration::from_secs(2),
        "offline --control hung for {:?}",
        started_at.elapsed()
    );
    let offline_err = String::from_utf8_lossy(&offline_ctrl.stderr);
    assert!(
        offline_err.contains("no running `--headless` daemon"),
        "expected actionable offline message: {offline_err}"
    );

    // 5. `--control` refuses setup flags, contradictory flags, and plaintext secrets without echo.
    let eq_secret = format!("--api-key={secret}");
    for bad_args in [
        vec!["--control"],
        vec!["--control", "--accept-terms", "--start-all"],
        vec!["--control", "--model", "other", "--start-all"],
        vec!["--control", "--start-all", "--stop-all"],
        vec!["--control", "--start-agent", "agent-1", "--stop-agent", "agent-1"],
        vec!["--control", "--start-agent", "agent-1", "--stop-on-no-work", "--keep-running"],
        vec!["--control", "--api-key", secret],
        vec!["--control", eq_secret.as_str()],
    ] {
        let out = base_cmd(&scratch)
            .args(&bad_args)
            .output()
            .expect("bad --control");
        assert!(!out.status.success(), "unexpected success for {bad_args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "secret echoed for {bad_args:?}:\nstdout={stdout}\nstderr={stderr}"
        );
    }

    // 6. `--control-socket` refuses a group/world-accessible parent directory (`0755`).
    let open_dir = scratch.path().join("open-sock-dir");
    fs::create_dir_all(&open_dir).expect("open dir");
    fs::set_permissions(&open_dir, fs::Permissions::from_mode(0o755)).expect("0755");
    let bad_mode_sock = open_dir.join("headless.sock");
    let mode_out = base_cmd(&scratch)
        .args(["--headless", "--allow-idle", "--control-socket"])
        .arg(&bad_mode_sock)
        .output()
        .expect("bad mode sock");
    assert!(!mode_out.status.success());
    let mode_err = String::from_utf8_lossy(&mode_out.stderr);
    assert!(
        mode_err.contains("0700"),
        "expected 0700 directory refusal: {mode_err}"
    );

    // 7. `--control-socket` refuses to overwrite or unlink a pre-existing regular file.
    let priv_dir = scratch.path().join("priv-sock-dir");
    fs::create_dir_all(&priv_dir).expect("priv dir");
    fs::set_permissions(&priv_dir, fs::Permissions::from_mode(0o700)).expect("0700");
    let regular_file = priv_dir.join("not-a-socket.txt");
    fs::write(&regular_file, "preserve-this-file\n").expect("write regular file");
    let reg_out = base_cmd(&scratch)
        .args(["--headless", "--allow-idle", "--control-socket"])
        .arg(&regular_file)
        .output()
        .expect("regular file sock");
    assert!(!reg_out.status.success());
    assert_eq!(
        fs::read_to_string(&regular_file).expect("regular file intact"),
        "preserve-this-file\n",
        "ControlSocketGuard must never overwrite or delete a regular file"
    );

    // 7b. `--control-socket` refuses to follow a pre-existing symlink at `<socket>.cookie`.
    let cookie_target = scratch.path().join("do-not-clobber-via-cookie-symlink.txt");
    fs::write(&cookie_target, "keep-intact\n").expect("write cookie target");
    let sym_sock = priv_dir.join("sym-test.sock");
    let sym_cookie = priv_dir.join("sym-test.sock.cookie");
    std::os::unix::fs::symlink(&cookie_target, &sym_cookie).expect("symlink cookie");
    let sym_out = base_cmd(&scratch)
        .args(["--headless", "--allow-idle", "--control-socket"])
        .arg(&sym_sock)
        .output()
        .expect("symlink cookie sock");
    assert!(!sym_out.status.success());
    assert_eq!(
        fs::read_to_string(&cookie_target).expect("cookie symlink target intact"),
        "keep-intact\n"
    );
    let _ = fs::remove_file(&sym_cookie);

    // 8. `--control-socket` refuses a path inside `<config-dir>/agents/`.
    let inside_agents = scratch.config().join("agents/agent-1/sock-dir");
    fs::create_dir_all(&inside_agents).expect("inside agents");
    fs::set_permissions(&inside_agents, fs::Permissions::from_mode(0o700)).expect("0700");
    let agents_sock = inside_agents.join("headless.sock");
    let agents_out = base_cmd(&scratch)
        .args(["--headless", "--allow-idle", "--control-socket"])
        .arg(&agents_sock)
        .output()
        .expect("inside agents sock");
    assert!(!agents_out.status.success());
    let agents_err = String::from_utf8_lossy(&agents_out.stderr);
    assert!(
        agents_err.contains("agents directory") || agents_err.contains("workspace"),
        "expected agents-dir overlap refusal: {agents_err}"
    );

    // 9. F1 regression: when Landlock confinement is active (`"Landlock"`), `--control-socket`
    //    inside the binary's directory (even when copied under `<config-dir>/bin` so it lies
    //    inside a protected root!) is refused before bind because Landlock grants read access
    //    to the running binary's parent directory.
    if st["confined"] == serde_json::json!(true)
        && st["confinement"]
            == serde_json::json!(gewu_multiverse::confinement::Method::Landlock.label())
    {
        if let Some(bin_parent) = Path::new(binary()).parent() {
            let probe_dir =
                bin_parent.join(format!("gewu-sock-overlap-probe-{}", std::process::id()));
            if fs::create_dir(&probe_dir).is_ok() {
                let _ = fs::set_permissions(&probe_dir, fs::Permissions::from_mode(0o700));
                let overlap_sock = probe_dir.join("headless.sock");
                let overlap_out = base_cmd(&scratch)
                    .args(["--headless", "--allow-idle", "--control-socket"])
                    .arg(&overlap_sock)
                    .output()
                    .expect("landlock overlap sock");
                let _ = fs::remove_dir_all(&probe_dir);
                assert!(!overlap_out.status.success());
                let overlap_err = String::from_utf8_lossy(&overlap_out.stderr);
                assert!(
                    overlap_err.contains("Landlock"),
                    "expected Landlock overlap refusal: {overlap_err}"
                );
            }
        }

        let copied_bin_dir = scratch.config().join("bin");
        let copied_sock_dir = copied_bin_dir.join("sock");
        fs::create_dir_all(&copied_sock_dir).expect("copied bin/sock dir");
        fs::set_permissions(&copied_bin_dir, fs::Permissions::from_mode(0o700)).expect("0700");
        fs::set_permissions(&copied_sock_dir, fs::Permissions::from_mode(0o700)).expect("0700");
        let copied_bin = copied_bin_dir.join("gewu-multiverse");
        fs::copy(binary(), &copied_bin).expect("copy binary into config/bin");
        fs::set_permissions(&copied_bin, fs::Permissions::from_mode(0o755)).expect("0755");
        let copied_sock = copied_sock_dir.join("headless.sock");
        let copied_out = Command::new(&copied_bin)
            .arg("--config-dir")
            .arg(scratch.config())
            .arg("--state-dir")
            .arg(scratch.state())
            .arg("--data-dir")
            .arg(scratch.data())
            .args(["--headless", "--allow-idle", "--control-socket"])
            .arg(&copied_sock)
            .stdin(Stdio::null())
            .output()
            .expect("copied binary landlock overlap");
        assert!(
            !copied_out.status.success(),
            "socket under binary dir inside config-dir must be refused under Landlock"
        );
        let copied_err = String::from_utf8_lossy(&copied_out.stderr);
        assert!(
            copied_err.contains("Landlock"),
            "expected Landlock overlap refusal for <config-dir>/bin/sock: {copied_err}"
        );
        assert!(!copied_sock.exists(), "refused socket must not be bound");
        let _ = fs::remove_dir_all(&copied_bin_dir);
    }

    // 10. F3 regression: a `--state-dir` whose `<state-dir>/headless.sock` exceeds Linux
    //     `SUN_LEN` (107 bytes) still binds its control socket via `/proc/self/fd/<dir_fd>`
    //     instead of failing with `SUN_LEN`, while an overlong socket leaf filename (> 90
    //     bytes) returns an actionable `--control-socket` message.
    let long_state = scratch
        .path()
        .join("deeply-nested-state-path-segment-alpha-0123456789")
        .join("deeply-nested-state-path-segment-beta-0123456789")
        .join("deeply-nested-state-path-segment-gamma-0123456789");
    assert!(
        long_state.join("headless.sock").as_os_str().len() > 130,
        "expected >130 byte socket path"
    );
    let bad_forgejo = scratch.path().join("bad-forgejo-long");
    fs::write(&bad_forgejo, b"invalid-forgejo\n").expect("bad forgejo");
    let long_out = Command::new(binary())
        .arg("--config-dir")
        .arg(scratch.config())
        .arg("--state-dir")
        .arg(&long_state)
        .arg("--data-dir")
        .arg(scratch.data())
        .args(["--headless", "--allow-idle"])
        .env("GEWU_FORGEJO_BINARY", &bad_forgejo)
        .stdin(Stdio::null())
        .output()
        .expect("long state-dir headless");
    assert!(!long_out.status.success());
    let long_err = String::from_utf8_lossy(&long_out.stderr);
    assert!(
        !long_err.contains("SUN_LEN") && long_err.contains("bundled Forgejo failed to start"),
        "long --state-dir should bind control socket cleanly and reach Forgejo start: {long_err}"
    );

    let overlong_leaf = priv_dir.join("s".repeat(100));
    let leaf_out = base_cmd(&scratch)
        .args(["--headless", "--allow-idle", "--control-socket"])
        .arg(&overlong_leaf)
        .output()
        .expect("overlong socket filename");
    assert!(!leaf_out.status.success());
    let leaf_err = String::from_utf8_lossy(&leaf_out.stderr);
    assert!(
        leaf_err.contains("--control-socket") && leaf_err.contains("limit"),
        "expected actionable socket length limit error: {leaf_err}"
    );
}

#[test]
#[ignore = "live Forgejo idle-standby + --control + Landlock + --stop-on-no-work E2E: run with GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64 cargo test --test headless-daemon -- --ignored --nocapture"]
fn live_idle_warm_standby_runtime_control_landlock_isolation_and_stop_on_no_work() {
    let _serial = LIVE_FORGEJO_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let forgejo_bin = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("set GEWU_FORGEJO_BINARY to the pinned forgejo-17.0.0-linux-amd64 binary")
    }));
    assert!(forgejo_bin.is_file());

    let scratch = Scratch::new("live-idle-control");
    let secret = "sk-idle-warm-secret-key-4242";
    let key_file = scratch.path().join("model.key");
    fs::write(&key_file, format!("{secret}\n")).expect("key");
    let initial_inst = scratch.path().join("initial.md");
    fs::write(&initial_inst, "Initial placeholder instruction.\n").expect("initial inst");

    let sock_path = scratch.state().join("headless.sock");
    let cookie_path = scratch.state().join("headless.sock.cookie");
    let sock_probe_py = concat!(
        "import json, socket, subprocess, sys\n",
        "sock_p, cookie_p, bin_p, cfg_p, state_p, data_p = sys.argv[1:7]\n",
        "try:\n",
        "    open(cookie_p, \"rb\").read()\n",
        "    print(\"COOKIE_READ_ALLOWED\")\n",
        "except Exception as e:\n",
        "    print(\"COOKIE_READ_BLOCKED:\" + type(e).__name__)\n",
        "res = subprocess.run([bin_p, \"--config-dir\", cfg_p, \"--state-dir\", state_p, \"--data-dir\", data_p, \"--control\", \"--stop-all\"], capture_output=True, text=True)\n",
        "print(\"CLI_CONTROL_ALLOWED\" if res.returncode == 0 else \"CLI_CONTROL_BLOCKED\")\n",
        "s = socket.socket(socket.AF_UNIX)\n",
        "try:\n",
        "    s.connect(sock_p)\n",
        "    s.sendall(json.dumps({\"stop_all\": True}).encode() + b\"\\n\")\n",
        "    resp = json.loads(s.makefile(\"r\").readline())\n",
        "    print(\"CONTROL_BYPASS_ALLOWED\" if resp.get(\"ok\") else \"RAW_CONTROL_REJECTED\")\n",
        "except Exception as e:\n",
        "    print(\"RAW_CONNECT_BLOCKED:\" + type(e).__name__)\n",
    );
    let sock_probe_cmd = format!(
        "python3 -c '{}' '{}' '{}' '{}' '{}' '{}' '{}'",
        sock_probe_py,
        sock_path.display(),
        cookie_path.display(),
        binary(),
        scratch.config().display(),
        scratch.state().display(),
        scratch.data().display(),
    );
    let shell_args = serde_json::json!({ "command": sock_probe_cmd }).to_string();
    let comment_args = serde_json::json!({
        "repository": "team/workspace",
        "number": 1,
        "body": "Processed event #1 in woken session."
    })
    .to_string();

    // Scripted replies:
    // - Session #1 (3 replies):
    //   1) shell tool probing `<state-dir>/headless.sock` (must be blocked by Landlock!)
    //   2) forgejo_comment on team/workspace#1 (real remote write -> SessionEnd::Succeeded)
    //   3) final summary text ("Completed Issue #1.")
    // - Session #2 (2 replies, started automatically because Session #1 Succeeded):
    //   4) forgejo_list_assigned_issues (read-only)
    //   5) final summary text ("No remaining work; nothing to do.") -> SessionEnd::NoWork -> immediate auto-park!
    // - Agent-2 manual wake/stop check (1 slow reply so we can exercise live `--control --stop-agent agent-2`):
    let mut slow_agent2_reply = StubReply::json(
        r#"{"choices":[{"message":{"role":"assistant","content":"No assigned issues found; nothing to do."}}]}"#,
    );
    slow_agent2_reply.delay = Duration::from_millis(100);

    let replies = vec![
        StubReply::json(
            serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_sock_probe",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": shell_args,
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        ),
        StubReply::json(
            serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_comment_1",
                            "type": "function",
                            "function": {
                                "name": "forgejo_comment",
                                "arguments": comment_args,
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        ),
        StubReply::json(
            r#"{"choices":[{"message":{"role":"assistant","content":"Completed Issue #1."}}]}"#,
        ),
        StubReply::json(
            serde_json::json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_list_2",
                            "type": "function",
                            "function": {
                                "name": "forgejo_list_assigned_issues",
                                "arguments": "{}",
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        ),
        StubReply::json(
            r#"{"choices":[{"message":{"role":"assistant","content":"No assigned issues left; nothing to do."}}]}"#,
        ),
        slow_agent2_reply,
    ];
    let stub = Stub::start(replies);

    // Configure 2 builtin agents (`agent-1` has an instruction, `agent-2` has an empty instruction),
    // BOTH left `stopped`.
    let cfg = base_cmd(&scratch)
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
            "stub-idle-control",
            "--api-key-file",
        ])
        .arg(&key_file)
        .arg("--instruction-file")
        .arg(&initial_inst)
        .args(["--stop-all"])
        .output()
        .expect("configure");
    assert!(
        cfg.status.success(),
        "{}",
        String::from_utf8_lossy(&cfg.stderr)
    );
    let cfg_agent2 = base_cmd(&scratch)
        .args(["--configure", "--agent", "agent-2", "--program", "builtin"])
        .output()
        .expect("configure agent-2 builtin");
    assert!(
        cfg_agent2.status.success(),
        "{}",
        String::from_utf8_lossy(&cfg_agent2.stderr)
    );

    // Spawn `gewu-multiverse --headless --allow-idle` detached from any TTY.
    use std::os::unix::process::CommandExt;
    let mut daemon = DaemonGuard::new(
        base_cmd(&scratch)
            .args(["--headless", "--allow-idle"])
            .env("GEWU_FORGEJO_BINARY", &forgejo_bin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("idle-warm headless starts"),
    );
    let daemon_pid = daemon.id();

    // Wait until `healthy == true`: Forgejo running, `team/workspace` created, and both agent logins provisioned!
    let deadline = Instant::now() + Duration::from_secs(180);
    let ready_status = loop {
        if let Some(status) = daemon.try_wait().expect("try_wait") {
            let out = daemon.wait_with_output().expect("output");
            panic!(
                "idle headless daemon exited early ({status}):\n{}",
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
            break v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for --headless --allow-idle healthy readiness"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    assert_eq!(ready_status["idle_standby"], serde_json::json!(true));
    assert_eq!(ready_status["ready_for_headless"], serde_json::json!(false));
    assert_eq!(ready_status["ready_for_idle_headless"], serde_json::json!(true));
    assert_eq!(ready_status["agents"][0]["provisioning"], serde_json::json!("ready"));
    assert_eq!(ready_status["agents"][1]["provisioning"], serde_json::json!("ready"));
    let forgejo_url = ready_status["forgejo"]["url"]
        .as_str()
        .expect("forgejo url")
        .to_owned();
    let confined = ready_status["confined"].as_bool().unwrap_or(false);
    let confinement_kind = ready_status["confinement"]
        .as_str()
        .unwrap_or("none")
        .to_owned();

    // Verify 0600 permissions on `headless.sock`, `headless.sock.cookie`, and `headless-status.json`.
    let sock_meta = fs::symlink_metadata(&sock_path).expect("headless.sock exists");
    assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600);
    let cookie_meta = fs::symlink_metadata(&cookie_path).expect("headless.sock.cookie exists");
    assert_eq!(cookie_meta.permissions().mode() & 0o777, 0o600);
    assert!(
        fs::read_to_string(&cookie_path)
            .expect("cookie text")
            .trim()
            .len()
            >= 64
    );
    let snap_meta =
        fs::symlink_metadata(scratch.state().join("headless-status.json")).expect("snapshot");
    assert_eq!(snap_meta.permissions().mode() & 0o777, 0o600);

    let pid_path = bundled_forgejo::InstancePaths::under(&scratch.data())
        .root
        .join(bundled_forgejo::PID_FILE);
    let forgejo_pid_before: u32 = fs::read_to_string(&pid_path)
        .expect("forgejo.pid")
        .trim()
        .parse()
        .expect("u32 pid");
    assert!(process_alive(forgejo_pid_before));

    // Hold warm standby for 3.5s and create Issue #1 in `team/workspace` in < 1s:
    // strictly ZERO model requests may fire while all agents are stopped!
    std::thread::sleep(Duration::from_millis(3500));
    assert_eq!(
        stub.requests().len(),
        0,
        "idle-warm standby made LLM requests while all agents were stopped!"
    );

    let creds_path = scratch.config().join("credentials.toml");
    let agent1_token =
        gewu_multiverse::credentials::forgejo_agent_token_optional(&creds_path, "agent-1")
            .expect("read creds")
            .expect("agent-1 forgejo token provisioned");
    let issue_started = Instant::now();
    let issue_payload = serde_json::json!({
        "title": "Event-driven task #1",
        "body": "Please handle event #1.",
        "assignees": ["gewu-agent-1"]
    })
    .to_string();
    let create_issue_resp = ureq::post(&format!("{forgejo_url}/api/v1/repos/team/workspace/issues"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("token {}", agent1_token.expose()))
        .send(&issue_payload)
        .expect("create Forgejo issue #1");
    assert!(
        (200..=299).contains(&create_issue_resp.status().as_u16()),
        "unexpected create issue status: {}",
        create_issue_resp.status()
    );
    assert!(
        issue_started.elapsed() < Duration::from_secs(1),
        "warm Forgejo issue creation took {:?} (>= 1s)",
        issue_started.elapsed()
    );
    assert_eq!(stub.requests().len(), 0, "still 0 LLM requests before --control wake");

    // Precondition validation on `--control`: starting `agent-2` (empty instruction) fails
    // non-zero without starting `agent-2` or disturbing the running daemon.
    let bad_wake = base_cmd(&scratch)
        .args(["--control", "--start-agent", "agent-2"])
        .output()
        .expect("bad wake agent-2");
    assert!(!bad_wake.status.success());
    assert!(
        String::from_utf8_lossy(&bad_wake.stderr).contains("instruction is empty"),
        "{}",
        String::from_utf8_lossy(&bad_wake.stderr)
    );

    // Hot-update `agent-1`'s `instruction.md` on disk WITHOUT restarting the daemon, then wake
    // `agent-1` with `--control --start-agent agent-1 --stop-on-no-work --json`.
    let agent1_inst_path = scratch.config().join("agents/agent-1/instruction.md");
    fs::write(
        &agent1_inst_path,
        "HOT-REFRESHED-INSTRUCTION-9001: process Issue #1 in team/workspace.\n",
    )
    .expect("hot update instruction.md");

    let wake_started = Instant::now();
    let wake_out = base_cmd(&scratch)
        .args([
            "--control",
            "--start-agent",
            "agent-1",
            "--stop-on-no-work",
            "--json",
        ])
        .output()
        .expect("wake agent-1");
    assert!(
        wake_out.status.success(),
        "wake failed:\n{}",
        String::from_utf8_lossy(&wake_out.stderr)
    );
    assert!(
        wake_started.elapsed() < Duration::from_secs(1),
        "--control wake took {:?}",
        wake_started.elapsed()
    );
    let wake_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&wake_out.stdout)).expect("wake json");
    assert_eq!(wake_json["ok"], serde_json::json!(true));
    assert_eq!(
        wake_json["status"]["idle_standby"],
        serde_json::json!(false)
    );
    assert_eq!(
        wake_json["status"]["agents"][0]["desired"],
        serde_json::json!("started")
    );
    assert_eq!(
        wake_json["status"]["agents"][0]["stop_on_no_work"],
        serde_json::json!(true)
    );

    // Wait for `agent-1` to complete Session #1 (`Succeeded` via `forgejo_comment`), automatically
    // advance into Session #2, finish Session #2 as `NoWork`, and auto-park back to `stopped`!
    // Must complete well before `NO_WORK_DELAY` (20s) would have scheduled a 3rd run.
    let park_deadline = Instant::now() + Duration::from_secs(35);
    let parked_status = loop {
        if let Some(status) = daemon.try_wait().expect("try_wait") {
            let out = daemon.wait_with_output().expect("output");
            panic!(
                "daemon exited unexpectedly during --stop-on-no-work run ({status}):\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let st = base_cmd(&scratch)
            .args(["--status", "--json"])
            .output()
            .expect("status");
        if st.status.success()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&st.stdout))
            && v["agents"][0]["desired"] == serde_json::json!("stopped")
            && v["agents"][0]["runs_total"].as_u64().unwrap_or(0) >= 2
        {
            break v;
        }
        assert!(
            Instant::now() < park_deadline,
            "timed out waiting for agent-1 multi-run completion + --stop-on-no-work auto-park"
        );
        std::thread::sleep(Duration::from_millis(250));
    };

    assert_eq!(parked_status["idle_standby"], serde_json::json!(true));
    assert_eq!(parked_status["healthy"], serde_json::json!(true));
    assert_eq!(parked_status["agents"][0]["runs_total"], serde_json::json!(2));
    assert_eq!(
        parked_status["agents"][0]["stop_on_no_work"],
        serde_json::json!(false)
    );

    // Verify `multiverse.toml` persisted `desired = "stopped"` and zero extra model calls occur.
    let saved_toml =
        fs::read_to_string(scratch.config().join("multiverse.toml")).expect("multiverse.toml");
    assert!(
        !saved_toml.contains(r#"desired = "started""#),
        "auto-park must persist desired = stopped in multiverse.toml:\n{saved_toml}"
    );
    std::thread::sleep(Duration::from_secs(4));
    let recorded_reqs = stub.requests();
    assert_eq!(
        recorded_reqs.len(),
        5,
        "expected exactly 5 model turns across 2 sessions (0 idle burn after auto-park), got {}",
        recorded_reqs.len()
    );

    // Proof 1: hot-refreshed `instruction.md` was delivered to Session #1 without restarting the daemon!
    assert!(
        recorded_reqs[0]
            .body
            .contains("HOT-REFRESHED-INSTRUCTION-9001"),
        "Session #1 did not receive hot-refreshed instruction.md:\n{}",
        recorded_reqs[0].body
    );

    // Proof 2: Confined `shell` tool cannot read `headless.sock.cookie`, cannot run
    // `gewu-multiverse --control`, and raw `AF_UNIX` `connect` without the cookie is rejected!
    let turn2_json: serde_json::Value =
        serde_json::from_str(&recorded_reqs[1].body).expect("turn 2 request json");
    let tool_output = turn2_json["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|m| m["role"] == serde_json::json!("tool"))
        .and_then(|m| m["content"].as_str())
        .expect("tool output message");
    assert!(
        !tool_output.contains("COOKIE_READ_ALLOWED")
            && !tool_output.contains("CLI_CONTROL_ALLOWED")
            && !tool_output.contains("CONTROL_BYPASS_ALLOWED"),
        "confined agent shell bypassed control socket isolation! tool output:\n{tool_output}"
    );
    if confined
        && confinement_kind == gewu_multiverse::confinement::Method::Landlock.label()
    {
        assert!(
            tool_output.contains("COOKIE_READ_BLOCKED:PermissionError")
                && tool_output.contains("CLI_CONTROL_BLOCKED")
                && (tool_output.contains("RAW_CONTROL_REJECTED")
                    || tool_output.contains("RAW_CONNECT_BLOCKED:")),
            "expected Landlock cookie PermissionError + CLI block + raw socket rejection, got:\n{tool_output}"
        );
    }

    // Proof 3: `gewu-agent-1`'s comment on `team/workspace#1` was genuinely written to Forgejo!
    let mut comments_resp = ureq::get(&format!(
        "{forgejo_url}/api/v1/repos/team/workspace/issues/1/comments"
    ))
    .header("Authorization", &format!("token {}", agent1_token.expose()))
    .call()
    .expect("read issue #1 comments");
    let comments_body = comments_resp
        .body_mut()
        .read_to_string()
        .expect("comments body");
    assert!(
        comments_body.contains("Processed event #1 in woken session."),
        "missing agent-1 Forgejo comment: {comments_body}"
    );

    // Proof 4 (F5 regression): Multi-target `--control --start-all` rolls back `agent-1` if
    // `agent-2` hits a runtime `Command::Start` block (`removal-pending` marker), leaving zero partial state!
    let agent2_inst_path = scratch.config().join("agents/agent-2/instruction.md");
    fs::write(&agent2_inst_path, "Inspect agent-2 queue.\n").expect("agent-2 inst");
    let removed_marker = scratch.state().join("agent-2/removal-pending");
    fs::create_dir_all(removed_marker.parent().expect("parent")).expect("agent-2 state dir");
    fs::write(&removed_marker, b"1\n").expect("write removed marker");
    let atomic_fail = base_cmd(&scratch)
        .args(["--control", "--start-all"])
        .output()
        .expect("atomic --start-all failure");
    assert!(!atomic_fail.status.success());
    fs::remove_file(&removed_marker).expect("remove temporary marker");
    let post_rollback_st = base_cmd(&scratch)
        .args(["--status", "--json"])
        .output()
        .expect("status after rollback");
    let post_rollback_val: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&post_rollback_st.stdout)).expect("json");
    assert_eq!(
        post_rollback_val["agents"][0]["desired"],
        serde_json::json!("stopped"),
        "agent-1 must be rolled back to stopped when a later --start-all target blocks"
    );
    assert_eq!(
        post_rollback_val["idle_standby"],
        serde_json::json!(true)
    );
    assert_eq!(
        stub.requests().len(),
        5,
        "rolled-back --start-all must not dispatch any LLM request"
    );

    // Proof 5 (F4 regression + live start/stop): Start `agent-2` WITHOUT `--stop-on-no-work`;
    // once its single turn finishes as `Restarting (no-work)` inside the 20s `NO_WORK_DELAY`
    // window, send daemon-wide `--control --stop-on-no-work --json` and verify `agent-2` parks
    // IMMEDIATELY to `stopped` (`runs_total == 1`, zero extra LLM turn), while Forgejo's PID
    // remains unchanged!
    let wake2 = base_cmd(&scratch)
        .args(["--control", "--start-agent", "agent-2"])
        .output()
        .expect("wake agent-2");
    assert!(
        wake2.status.success(),
        "{}",
        String::from_utf8_lossy(&wake2.stderr)
    );
    let wait_restarting = Instant::now() + Duration::from_secs(20);
    loop {
        let st = base_cmd(&scratch)
            .args(["--status", "--json"])
            .output()
            .expect("status");
        if st.status.success()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&st.stdout))
            && v["agents"][1]["session"]
                .as_str()
                .is_some_and(|s| s.starts_with("restarting (no-work"))
        {
            break;
        }
        assert!(
            Instant::now() < wait_restarting,
            "timed out waiting for agent-2 to enter restarting (no-work) window"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let reqs_after_agent2_run1 = stub.requests().len();
    assert_eq!(reqs_after_agent2_run1, 6);

    let arm_global = base_cmd(&scratch)
        .args(["--control", "--stop-on-no-work", "--json"])
        .output()
        .expect("global --stop-on-no-work");
    assert!(
        arm_global.status.success(),
        "{}",
        String::from_utf8_lossy(&arm_global.stderr)
    );
    let arm_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&arm_global.stdout)).expect("arm json");
    assert_eq!(
        arm_json["status"]["agents"][1]["desired"],
        serde_json::json!("stopped"),
        "F4: global --stop-on-no-work must immediately park an agent already waiting in NoWork"
    );
    assert_eq!(
        arm_json["status"]["idle_standby"],
        serde_json::json!(true)
    );
    assert_eq!(stub.requests().len(), 6, "zero extra LLM requests after immediate park");
    let forgejo_pid_after: u32 = fs::read_to_string(&pid_path)
        .expect("forgejo.pid still present")
        .trim()
        .parse()
        .expect("u32 pid");
    assert_eq!(
        forgejo_pid_before, forgejo_pid_after,
        "Forgejo PID changed across live --control start/stop!"
    );
    assert!(process_alive(forgejo_pid_after));

    // Send SIGTERM to the daemon and verify exit 0, zero leaked secrets, Forgejo stopped, and
    // `headless.sock` unlinked!
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("SIGTERM");
    let out = daemon.wait_with_output().expect("daemon exits");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "daemon did not exit 0 on SIGTERM ({:?}):\n{stderr}",
        out.status
    );
    assert!(
        !stderr.contains(secret),
        "secret leaked in daemon stderr:\n{stderr}"
    );
    assert!(!process_alive(forgejo_pid_after));
    assert!(
        !sock_path.exists(),
        "headless.sock was not unlinked on clean shutdown"
    );
    assert!(
        !cookie_path.exists(),
        "headless.sock.cookie was not unlinked on clean shutdown"
    );
}
