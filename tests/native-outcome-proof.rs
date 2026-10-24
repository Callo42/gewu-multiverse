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
