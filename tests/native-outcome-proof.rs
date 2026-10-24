//! Live packaged-product proof for the two endings the engine causes: a stop and a
//! deadline. The product binary under test is the release ELF; the model is the
//! synthetic loopback server under `probes/`.
//!
//! Run with:
//!
//!   GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_MOCK_PORT=...
//!   GEWU_PROOF_TAG=new cargo test --test native-outcome-proof -- --ignored --nocapture
//!
//! No paid provider is contacted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{Command, EngineApi, ForgejoState, Provisioning};
use gewu_multiverse::harness::{self, InstallRecord, Program};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run;
use gewu_multiverse::transcript::{self, Record};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("gewu-outcome-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch root");
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("GEWU_PROOF_KEEP").is_some() {
            eprintln!("kept proof scratch: {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn install_pi(data_root: &Path, source: &Path) {
    let root = data_root.join("harnesses/pi/1.4.0");
    fs::create_dir_all(root.join("pi")).expect("fixture dir");
    let target = root.join("pi/pi");
    if !target.exists() {
        fs::hard_link(source, &target)
            .or_else(|_| fs::copy(source, &target).map(|_| ()))
            .expect("fixture binary");
    }
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("fixture mode");
    let record = InstallRecord {
        program: "pi".to_owned(),
        version: "1.4.0".to_owned(),
        sha256: "fixture".to_owned(),
        license: "MIT".to_owned(),
        entrypoint: "pi/pi".to_owned(),
        resources: None,
        installed_at: "2026-10-24T00:00:00Z".to_owned(),
    };
    let text = serde_json::to_string_pretty(&record).expect("record");
    gewu_multiverse::mode::write_private(root.join("install.json"), text.as_bytes())
        .expect("install record");
    assert!(harness::installed(data_root, Program::Pi).is_some());
}

fn wait_engine(
    engine: &mut impl EngineApi,
    what: &str,
    limit: Duration,
    mut ready: impl FnMut(&mut dyn EngineApi) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < limit {
        engine.tick();
        if ready(engine) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn write_mock_state(tag: &str, probe: &str, value: serde_json::Value) -> String {
    let root = PathBuf::from(
        std::env::var("GEWU_PROOF_ROOT").expect("GEWU_PROOF_ROOT names the probes workspace"),
    );
    fs::create_dir_all(root.join("probes/state")).expect("state dir");
    let _ = tag;
    fs::write(
        root.join("probes/state").join(format!("{probe}.json")),
        serde_json::to_vec(&value).expect("state JSON"),
    )
    .expect("state file");
    let _ = fs::remove_file(root.join("probes/state").join(format!("{probe}.count")));
    let port = fs::read_to_string(root.join("probes/mock.port"))
        .expect("the mock port file")
        .trim()
        .to_owned();
    format!("http://127.0.0.1:{port}/{probe}/v1")
}

fn open_engine(tag: &str, session_timeout: Duration) -> (Scratch, Layout, engine::Engine) {
    let _forgejo =
        PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").expect("GEWU_FORGEJO_BINARY"));
    let product = PathBuf::from(std::env::var("GEWU_PRODUCT_BINARY").expect("GEWU_PRODUCT_BINARY"));
    let harness_root = PathBuf::from(std::env::var("GEWU_PROOF_ROOT").expect("GEWU_PROOF_ROOT"));
    let scratch = Scratch::new(tag);
    let layout = Layout::rooted_at(scratch.0.join("config")).expect("layout");
    let data_root = scratch.0.join("data");
    install_pi(
        &data_root,
        &harness_root.join("probes/harness/pi/1.4.0/pi/pi"),
    );
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(1);
    options.session_timeout = session_timeout;
    let mut engine = engine::open_with(options).expect("engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start forgejo");
    wait_engine(&mut engine, "Forgejo", Duration::from_secs(240), |engine| {
        let board = engine.board();
        matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
    });
    (scratch, layout, engine)
}

fn create_agent(engine: &mut impl EngineApi) {
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(engine, "agent ready", Duration::from_secs(120), |engine| {
        let board = engine.board();
        board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
    });
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: "pi".to_owned(),
            protocol: Some("chat-completions".to_owned()),
        })
        .expect("Pi program");
}

fn wait_run_dir(engine: &mut impl EngineApi, layout: &Layout, limit: Duration) -> PathBuf {
    let start = Instant::now();
    let state = layout
        .agent("agent-1")
        .expect("agent")
        .state_dir()
        .to_path_buf();
    while start.elapsed() < limit {
        engine.tick();
        if let Ok(entries) = fs::read_dir(&state) {
            for entry in entries.flatten() {
                if entry.path().join(run::NATIVE_PLAN_FILE).is_file() {
                    return entry.path();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the native run never started");
}

fn wait_terminal(
    engine: &mut impl EngineApi,
    layout: &Layout,
    limit: Duration,
) -> (PathBuf, Record) {
    let start = Instant::now();
    let state = layout
        .agent("agent-1")
        .expect("agent")
        .state_dir()
        .to_path_buf();
    while start.elapsed() < limit {
        engine.tick();
        if let Ok(entries) = fs::read_dir(&state) {
            for entry in entries.flatten() {
                if let Some(record) = transcript::terminal_record(&entry.path()) {
                    return (entry.path(), record);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the run never wrote a terminal record");
}

fn set_endpoint(engine: &mut impl EngineApi, url: &str) {
    engine
        .send(Command::SetEndpoint {
            url: url.to_owned(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-proof-key".to_owned()),
        })
        .expect("endpoint");
}

#[test]
#[ignore = "live Forgejo + pinned Pi: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn a_cancelled_run_writes_a_stop_record_and_a_stopped_ending() {
    let (scratch, layout, mut engine) = open_engine("cancel", Duration::from_secs(30));
    let url = write_mock_state(
        "new",
        "proof-cancel",
        serde_json::json!({"scenario": "hang_body", "after": 1, "seconds": 600}),
    );
    set_endpoint(&mut engine, &url);
    create_agent(&mut engine);
    let instruction = engine
        .instruction_path("agent-1")
        .expect("instruction path");
    fs::write(&instruction, "Reply with one short sentence.\n").expect("instruction");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    let run_dir = wait_run_dir(&mut engine, &layout, Duration::from_secs(60));
    let _ = &run_dir;
    // Wait until the harness's request is in flight, then stop the run.
    let request_log = PathBuf::from(std::env::var("GEWU_PROOF_ROOT").expect("proof root"))
        .join("probes/reqlogs/proof-cancel.jsonl");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        engine.tick();
        if request_log.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop");
    let (run_dir, record) = wait_terminal(&mut engine, &layout, Duration::from_secs(60));
    match &record {
        Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, "stopped", "a stop was recorded as {record:?}");
            assert!(
                reason.as_deref().unwrap_or_default().contains("stopped"),
                "the stopped ending has no stop reason: {reason:?}"
            );
        }
        other => panic!("no terminal record: {other:?}"),
    }
    assert!(
        run_dir.join(run::STOPPED_FILE).is_file(),
        "a cancelled run left no stop record"
    );
    let stop: run::RunStopped =
        serde_json::from_str(&fs::read_to_string(run_dir.join(run::STOPPED_FILE)).unwrap())
            .expect("stop record JSON");
    assert!(stop.reason.contains("stopped"), "{}", stop.reason);
    eprintln!(
        "cancel proof: {} {:?} {}",
        run_dir.display(),
        record,
        stop.reason
    );
    drop(engine);
    let _ = fs::remove_dir_all(&scratch.0);
}

#[test]
#[ignore = "live Forgejo + pinned Pi: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn an_engine_deadline_writes_a_timed_out_ending_and_a_stop_record() {
    let (scratch, layout, mut engine) = open_engine("deadline", Duration::from_secs(2));
    let url = write_mock_state(
        "new",
        "proof-deadline",
        serde_json::json!({"scenario": "hang_body", "after": 1, "seconds": 600}),
    );
    set_endpoint(&mut engine, &url);
    create_agent(&mut engine);
    let instruction = engine
        .instruction_path("agent-1")
        .expect("instruction path");
    fs::write(&instruction, "Reply with one short sentence.\n").expect("instruction");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    let _run_dir = wait_run_dir(&mut engine, &layout, Duration::from_secs(60));
    let (run_dir, record) = wait_terminal(&mut engine, &layout, Duration::from_secs(45));
    match &record {
        Record::RunFinished {
            outcome, reason, ..
        } => {
            assert!(
                outcome == "timed_out" || outcome == "failed",
                "a deadline produced {record:?}"
            );
            let reason = reason.as_deref().unwrap_or_default();
            assert!(
                reason.contains("time limit")
                    || reason.contains("timed out")
                    || reason.contains("timeout"),
                "the deadline reason lost the time cause: {reason}"
            );
        }
        other => panic!("no terminal record: {other:?}"),
    }
    assert!(
        run_dir.join(run::STOPPED_FILE).is_file(),
        "a deadline run left no stop record"
    );
    eprintln!("deadline proof: {} {:?}", run_dir.display(), record);
    drop(engine);
    let _ = fs::remove_dir_all(&scratch.0);
}

#[test]
#[ignore = "live Forgejo + pinned Pi: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn an_engine_deadline_keeps_the_last_emitted_cause() {
    let (scratch, layout, mut engine) = open_engine("deadline-cause", Duration::from_secs(2));
    let url = write_mock_state(
        "new",
        "proof-deadline-cause",
        serde_json::json!({"scenario": "fail_then_hang", "status": 500, "after": 1, "seconds": 600}),
    );
    set_endpoint(&mut engine, &url);
    create_agent(&mut engine);
    let instruction = engine
        .instruction_path("agent-1")
        .expect("instruction path");
    fs::write(&instruction, "Reply with one short sentence.\n").expect("instruction");
    engine.tick();
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    let _run_dir = wait_run_dir(&mut engine, &layout, Duration::from_secs(60));
    let (run_dir, record) = wait_terminal(&mut engine, &layout, Duration::from_secs(45));
    match &record {
        Record::RunFinished {
            outcome, reason, ..
        } => {
            assert_eq!(
                outcome, "failed",
                "a failing harness was recorded as {record:?}"
            );
            let reason = reason.as_deref().unwrap_or_default();
            assert!(
                reason.contains("500") || reason.to_ascii_lowercase().contains("timed out"),
                "the deadline lost the emitted cause: {reason}"
            );
        }
        other => panic!("no terminal record: {other:?}"),
    }
    assert!(
        run_dir.join(run::STOPPED_FILE).is_file(),
        "a deadline run left no stop record"
    );
    let stop: run::RunStopped =
        serde_json::from_str(&fs::read_to_string(run_dir.join(run::STOPPED_FILE)).unwrap())
            .expect("stop record JSON");
    assert!(
        stop.reason.contains("500") || stop.reason.to_ascii_lowercase().contains("timed out"),
        "the stop record lost the cause: {}",
        stop.reason
    );
    eprintln!(
        "deadline cause proof: {} {:?} {}",
        run_dir.display(),
        record,
        stop.reason
    );
    drop(engine);
    let _ = fs::remove_dir_all(&scratch.0);
}

fn hardlink_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            hardlink_tree(&from, &to)?;
        } else if kind.is_symlink() {
            let destination = fs::read_link(&from)?;
            std::os::unix::fs::symlink(destination, &to)?;
        } else {
            fs::hard_link(&from, &to).or_else(|_| fs::copy(&from, &to).map(|_| ()))?;
        }
    }
    Ok(())
}

fn install_source(
    data_root: &Path,
    program: Program,
    source: &Path,
    version: &str,
    entrypoint: &str,
) {
    let root = data_root.join("harnesses").join(program.id()).join(version);
    fs::create_dir_all(&root).expect("harness root");
    hardlink_tree(source, &root).expect("harness tree");
    let record = InstallRecord {
        program: program.id().to_owned(),
        version: version.to_owned(),
        sha256: "fixture".to_owned(),
        license: program.license().to_owned(),
        entrypoint: entrypoint.to_owned(),
        resources: None,
        installed_at: "2026-10-24T00:00:00Z".to_owned(),
    };
    let text = serde_json::to_string_pretty(&record).expect("record");
    gewu_multiverse::mode::write_private(root.join("install.json"), text.as_bytes())
        .expect("install record");
    let executable = root.join(entrypoint);
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("fixture mode");
}

fn harness_source(harness: &str) -> (Program, String, String) {
    match harness {
        "pi" => (Program::Pi, "1.4.0".to_owned(), "pi/pi".to_owned()),
        "codex" => (Program::Codex, "0.164.0".to_owned(), "bin/codex".to_owned()),
        "dsh" => (
            Program::Dsh,
            "0.2.1-rc.2".to_owned(),
            "node_modules/.bin/dsh".to_owned(),
        ),
        "opencode" => (
            Program::Opencode,
            "2.1.4".to_owned(),
            "package/bin/opencode".to_owned(),
        ),
        other => panic!("unknown harness {other}"),
    }
}

fn open_engine_for(
    tag: &str,
    harness: &str,
    session_timeout: Duration,
) -> (Scratch, Layout, engine::Engine, String) {
    let _forgejo =
        PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").expect("GEWU_FORGEJO_BINARY"));
    let product = PathBuf::from(std::env::var("GEWU_PRODUCT_BINARY").expect("GEWU_PRODUCT_BINARY"));
    let proof_root = PathBuf::from(std::env::var("GEWU_PROOF_ROOT").expect("GEWU_PROOF_ROOT"));
    let scratch = Scratch::new(tag);
    let layout = Layout::rooted_at(scratch.0.join("config")).expect("layout");
    let data_root = scratch.0.join("data");
    let (program, version, entrypoint) = harness_source(harness);
    if program == Program::Pi {
        install_pi(
            &data_root,
            &proof_root.join("probes/harness/pi/1.4.0/pi/pi"),
        );
    } else {
        install_source(
            &data_root,
            program,
            &proof_root
                .join("probes/harness")
                .join(harness)
                .join(&version),
            &version,
            &entrypoint,
        );
    }
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(1);
    options.session_timeout = session_timeout;
    let mut engine = engine::open_with(options).expect("engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start forgejo");
    wait_engine(&mut engine, "Forgejo", Duration::from_secs(240), |engine| {
        let board = engine.board();
        matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
    });
    // Native selection snapshots the default connection, so set one first; the
    // per-run SetAgentModel below supplies the actual synthetic endpoint.
    engine
        .send(Command::SetEndpoint {
            url: "http://127.0.0.1:9/v1".to_owned(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-proof-key".to_owned()),
        })
        .expect("default endpoint");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(
        &mut engine,
        "agent ready",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    let protocol = if program == Program::Codex {
        "responses"
    } else {
        "chat-completions"
    };
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: program.id().to_owned(),
            protocol: Some(protocol.to_owned()),
        })
        .expect("program");
    if program == Program::Codex {
        // The host-wide bwrap preflight blocks Codex's default sandbox on this
        // machine; the explicit D106 compatibility choice keeps the product
        // Landlock boundary and lets the stop/deadline path run.
        engine
            .send(Command::SetAgentCodexSandbox {
                name: "agent-1".to_owned(),
                mode: "full-access".to_owned(),
            })
            .expect("codex compatibility sandbox");
    }
    (scratch, layout, engine, protocol.to_owned())
}

fn wait_run_dir_for(
    engine: &mut impl EngineApi,
    layout: &Layout,
    agent: &str,
    limit: Duration,
) -> PathBuf {
    let start = Instant::now();
    let state = layout
        .agent(agent)
        .expect("agent")
        .state_dir()
        .to_path_buf();
    while start.elapsed() < limit {
        engine.tick();
        if let Ok(entries) = fs::read_dir(&state) {
            for entry in entries.flatten() {
                if entry.path().join(run::NATIVE_PLAN_FILE).is_file() {
                    return entry.path();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the native run never started for {agent}");
}

fn wait_terminal_for(
    engine: &mut impl EngineApi,
    layout: &Layout,
    agent: &str,
    limit: Duration,
) -> (PathBuf, Record) {
    let start = Instant::now();
    let state = layout
        .agent(agent)
        .expect("agent")
        .state_dir()
        .to_path_buf();
    while start.elapsed() < limit {
        engine.tick();
        if let Ok(entries) = fs::read_dir(&state) {
            for entry in entries.flatten() {
                if let Some(record) = transcript::terminal_record(&entry.path()) {
                    return (entry.path(), record);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the run never wrote a terminal record for {agent}");
}

fn start_agent(
    engine: &mut impl EngineApi,
    layout: &Layout,
    agent: &str,
    probe: &str,
    protocol: &str,
    scenario: serde_json::Value,
) -> PathBuf {
    let url = write_mock_state("new", probe, scenario);
    engine
        .send(Command::SetAgentModel {
            name: agent.to_owned(),
            base_url: url,
            protocol: protocol.to_owned(),
            model: "synthetic-model".to_owned(),
            key: Some("gw-proof-key".to_owned()),
        })
        .expect("agent model");
    let instruction = engine.instruction_path(agent).expect("instruction path");
    fs::write(&instruction, "Reply with one short sentence.\n").expect("instruction");
    engine.tick();
    engine
        .send(Command::Start {
            name: agent.to_owned(),
        })
        .expect("start");
    wait_run_dir_for(engine, layout, agent, Duration::from_secs(60))
}

fn wait_for_request(probe: &str, limit: Duration) {
    let root = PathBuf::from(std::env::var("GEWU_PROOF_ROOT").expect("proof root"));
    let log = root.join("probes/reqlogs").join(format!("{probe}.jsonl"));
    let start = Instant::now();
    while start.elapsed() < limit {
        if log.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the harness never sent a request for {probe}");
}

#[test]
#[ignore = "live Forgejo + four pinned harnesses: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn every_harness_cancelled_run_writes_a_stop_record() {
    let only = std::env::var("GEWU_PROOF_ONLY").ok();
    for harness in ["pi", "codex", "dsh", "opencode"] {
        if only.as_deref().is_some_and(|only| only != harness) {
            continue;
        }
        let (scratch, layout, mut engine, protocol) = open_engine_for(
            &format!("cancel-{harness}"),
            harness,
            Duration::from_secs(30),
        );
        let probe = format!("proof-cancel-{harness}");
        let _run_dir = start_agent(
            &mut engine,
            &layout,
            "agent-1",
            &probe,
            &protocol,
            serde_json::json!({"scenario": "hang_body", "after": 1, "seconds": 600}),
        );
        wait_for_request(&probe, Duration::from_secs(30));
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("stop");
        let (run_dir, record) =
            wait_terminal_for(&mut engine, &layout, "agent-1", Duration::from_secs(60));
        match &record {
            Record::RunFinished { outcome, .. } => {
                assert_eq!(outcome, "stopped", "{harness} cancel produced {record:?}");
            }
            other => panic!("{harness} cancel: no terminal record: {other:?}"),
        }
        assert!(
            run_dir.join(run::STOPPED_FILE).is_file(),
            "{harness} cancel left no stop record"
        );
        let stop: run::RunStopped =
            serde_json::from_str(&fs::read_to_string(run_dir.join(run::STOPPED_FILE)).unwrap())
                .expect("stop JSON");
        assert!(
            stop.reason.contains("stopped"),
            "{harness} stop reason: {}",
            stop.reason
        );
        eprintln!("cancel {harness}: {record:?} {}", stop.reason);
        drop(engine);
        let _ = fs::remove_dir_all(&scratch.0);
    }
}

#[test]
#[ignore = "live Forgejo + four pinned harnesses: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn every_harness_engine_deadline_writes_a_stop_record() {
    let only = std::env::var("GEWU_PROOF_ONLY").ok();
    for harness in ["pi", "codex", "dsh", "opencode"] {
        if only.as_deref().is_some_and(|only| only != harness) {
            continue;
        }
        let (scratch, layout, mut engine, protocol) = open_engine_for(
            &format!("deadline-{harness}"),
            harness,
            Duration::from_secs(2),
        );
        let probe = format!("proof-deadline-{harness}");
        let _run_dir = start_agent(
            &mut engine,
            &layout,
            "agent-1",
            &probe,
            &protocol,
            serde_json::json!({"scenario": "hang_body", "after": 1, "seconds": 600}),
        );
        let (run_dir, record) =
            wait_terminal_for(&mut engine, &layout, "agent-1", Duration::from_secs(60));
        match &record {
            Record::RunFinished {
                outcome, reason, ..
            } => {
                assert!(
                    outcome == "timed_out" || outcome == "failed",
                    "{harness} deadline produced {record:?}"
                );
                let reason = reason.as_deref().unwrap_or_default();
                assert!(
                    reason.contains("time limit")
                        || reason.contains("timed out")
                        || reason.contains("timeout"),
                    "{harness} deadline reason: {reason}"
                );
            }
            other => panic!("{harness} deadline: no terminal record: {other:?}"),
        }
        assert!(
            run_dir.join(run::STOPPED_FILE).is_file(),
            "{harness} deadline left no stop record"
        );
        eprintln!("deadline {harness}: {record:?}");
        drop(engine);
        let _ = fs::remove_dir_all(&scratch.0);
    }
}

fn terminal_records(run_dir: &Path) -> Vec<Record> {
    transcript::read(run_dir)
        .expect("the run transcript reads")
        .records
        .into_iter()
        .filter(|record| matches!(record, Record::RunFinished { .. }))
        .collect()
}

#[test]
#[ignore = "live Forgejo + pinned Pi: run with GEWU_FORGEJO_BINARY=... GEWU_PRODUCT_BINARY=... GEWU_PROOF_ROOT=..."]
fn a_stop_after_the_runs_terminal_record_does_not_replace_it() {
    for harness in ["pi", "opencode"] {
        let (scratch, layout, mut engine, protocol) = open_engine_for(
            &format!("finish-then-stop-{harness}"),
            harness,
            Duration::from_secs(30),
        );
        let probe = format!("proof-finish-then-stop-{harness}");
        let run_dir = start_agent(
            &mut engine,
            &layout,
            "agent-1",
            &probe,
            &protocol,
            serde_json::json!({"scenario": "ok", "text": "probe ok"}),
        );
        // Wait for the child to write its own ending without ticking the engine. The
        // engine still holds the run as running, exactly the Main journey's race.
        let start = Instant::now();
        while terminal_records(&run_dir).is_empty() && start.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = terminal_records(&run_dir);
        assert_eq!(
            before.len(),
            1,
            "{harness} did not write exactly one ending"
        );
        let before_outcome = match &before[0] {
            Record::RunFinished { outcome, .. } => outcome.clone(),
            other => panic!("unexpected ending: {other:?}"),
        };
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("first stop");
        // The first Stop sampled Terminal; a second Stop must not re-open the
        // ordering decision after the child's ending.
        engine
            .send(Command::Stop {
                name: "agent-1".to_owned(),
            })
            .expect("second stop");
        let (same_dir, record) =
            wait_terminal_for(&mut engine, &layout, "agent-1", Duration::from_secs(60));
        assert_eq!(same_dir, run_dir);
        match &record {
            Record::RunFinished { outcome, .. } => {
                assert_eq!(
                    *outcome, before_outcome,
                    "{harness}: the stop replaced the run's outcome"
                );
            }
            other => panic!("{harness}: no terminal record: {other:?}"),
        }
        let after = terminal_records(&run_dir);
        assert_eq!(
            after.len(),
            1,
            "{harness}: the stop appended a second ending: {after:?}"
        );
        assert!(
            !run_dir.join(run::STOPPED_FILE).is_file(),
            "{harness}: a stop after the ending wrote a false stop record"
        );
        eprintln!("finish-then-stop {harness}: {record:?}");
        drop(engine);
        let _ = fs::remove_dir_all(&scratch.0);
    }
}
