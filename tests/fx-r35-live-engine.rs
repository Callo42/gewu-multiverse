//! fx-r35 live engine evidence: the real engine, the real bundled Forgejo, the real
//! `--run-agent` supervisor and a real sleeping shell. A loopback model names the
//! sleeping command. No TUI (this host's execution sandbox denies pseudo-terminals);
//! the renderer evidence is separate.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::engine::settings::{self, Settings};
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{
    Command as EngineCommand, Desired, EngineApi, Provisioning, SessionEnd, SessionState,
};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run;
use gewu_multiverse::test_stub::{Stub, StubReply};

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-live-engine-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch");
    root
}

fn wait_until(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn install_forgejo(data_root: &Path) {
    let source = std::env::var_os("GEWU_LIVE_FORGEJO")
        .map(PathBuf::from)
        .expect("set GEWU_LIVE_FORGEJO to the verified forgejo-17.0.0-linux-amd64 bytes");
    assert!(source.is_file(), "the declared Forgejo bytes are absent");
    let dir = bundled_forgejo::forgejo_dir(data_root);
    fs::create_dir_all(&dir).expect("forgejo dir");
    let target = bundled_forgejo::binary_path(data_root);
    fs::copy(source, &target).expect("copy the exact Forgejo bytes");
    let mut mode = fs::metadata(&target).expect("mode").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(&target, mode).expect("executable");
}

fn tool_call(command: &str) -> String {
    serde_json::json!({
        "model": "live-fixture",
        "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_sleep", "type": "function",
                    "function": {"name": "shell",
                        "arguments": serde_json::json!({"command": command}).to_string()}}]}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
    })
    .to_string()
}

fn final_reply() -> String {
    serde_json::json!({
        "model": "live-fixture",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "The wait is over."}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
    })
    .to_string()
}

fn seed(root: &Path, data_root: &Path, endpoint: &str) -> Layout {
    seed_with_instruction(
        root,
        data_root,
        endpoint,
        "sleeprun: wait for three hundred seconds.\n",
    )
}

fn seed_with_instruction(
    root: &Path,
    data_root: &Path,
    endpoint: &str,
    instruction: &str,
) -> Layout {
    let layout = Layout::rooted_at(root.to_path_buf()).expect("layout");
    layout.ensure_root().expect("root");
    let paths = layout.ensure_agent("agent-1").expect("agent paths");
    fs::write(paths.instruction(), instruction).expect("instruction");
    fs::create_dir_all(paths.skills_dir()).expect("skills");
    let mut settings = Settings {
        terms_acknowledged: true,
        terms_version: engine::TERMS_VERSION,
        ..Default::default()
    };
    settings.endpoint.url = endpoint.to_owned();
    settings.endpoint.model = "live-fixture".to_owned();
    settings.forgejo.admin_login = "owner".to_owned();
    settings
        .add_agent("agent-1", Desired::Stopped)
        .expect("agent");
    settings.save(&settings::path_in(root)).expect("settings");
    let credentials = root.join("credentials.toml");
    gewu_multiverse::mode::write_private(
        &credentials,
        b"model_key = \"gw-synthetic-live-not-a-real-key\"\n",
    )
    .expect("credentials");
    fs::create_dir_all(data_root).expect("data root");
    install_forgejo(data_root);
    layout
}

fn options(layout: &Layout, data_root: &Path) -> EngineOptions {
    let mut options = EngineOptions::new(layout.clone(), data_root.to_path_buf()).expect("options");
    // An integration test's own executable is the test harness, not the product:
    // point the real spawner at the product binary built from this exact tree.
    let product = std::env::var_os("GEWU_LIVE_PRODUCT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse")));
    assert!(
        product.is_file(),
        "the product binary is absent: {}",
        product.display()
    );
    options.product_binary = product;
    options.auto_start_forgejo = true;
    options.ready_timeout = Duration::from_secs(120);
    options
}

fn wait_ready(engine: &mut engine::Engine, limit: Duration) -> bool {
    wait_until(limit, || {
        engine.tick();
        let board = engine.board();
        board.forgejo.state == gewu_multiverse::engine_api::ForgejoState::Running
            && board.forgejo.repositories > 0
            && board
                .agents
                .first()
                .is_some_and(|agent| matches!(agent.provisioning, Provisioning::Ready))
    })
}

fn newest_run_dir(layout: &Layout) -> Option<PathBuf> {
    let mut runs = run::runs_of_agent(layout.state_root(), "agent-1");
    runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    runs.pop().map(|summary| summary.dir)
}

fn group_members(pgid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(state) = fields.next().and_then(|state| state.chars().next()) else {
            continue;
        };
        let _ppid = fields.next();
        let Some(group) = fields.next().and_then(|group| group.parse::<u32>().ok()) else {
            continue;
        };
        if state != 'Z' && group == pgid {
            found.push(pid);
        }
    }
    found
}

fn shell_pgid(run_dir: &Path) -> Option<u32> {
    fs::read_to_string(run_dir.join(run::SHELL_PGID_FILE))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// The real engine normal stop: start, sleep in a real shell, stop, inspect the
/// durable ending and handles, reopen and start again.
#[test]
#[ignore = "live: needs verified Forgejo bytes via GEWU_LIVE_FORGEJO"]
fn live_engine_normal_stop_and_reopen() {
    let root = scratch("engine-normal");
    let data_root = root.join("data");
    let stub = Stub::start(vec![
        StubReply::json(tool_call("sleep 300")),
        StubReply::json(final_reply()),
    ]);
    let layout = seed(&root, &data_root, &stub.url());
    let mut engine = engine::open_with(options(&layout, &data_root)).expect("engine opens");
    assert!(
        wait_ready(&mut engine, Duration::from_secs(150)),
        "the real Forgejo did not become ready: {:?}",
        engine.board().forgejo
    );
    engine
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start");
    assert!(
        wait_until(Duration::from_secs(60), || {
            engine.tick();
            matches!(
                engine.board().agents[0].session,
                SessionState::Running { .. }
            )
        }),
        "the real run never started: {:?}",
        engine.board().agents[0].session
    );
    let run_dir = wait_until(Duration::from_secs(30), || {
        newest_run_dir(&layout).is_some_and(|dir| shell_pgid(&dir).is_some())
    });
    assert!(run_dir, "the run shell record never appeared");
    let run_dir = newest_run_dir(&layout).expect("the run directory");
    let pgid = shell_pgid(&run_dir).expect("shell pgid");
    assert!(
        !group_members(pgid).is_empty(),
        "the sleeping shell is not alive"
    );
    engine
        .send(EngineCommand::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("Stop");
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine.tick();
            let board = engine.board();
            board.agents[0].desired == Desired::Stopped
                && matches!(board.agents[0].session, SessionState::Idle)
                && board.agents[0].leftover.is_none()
        }),
        "the normal stop did not settle: {:?}",
        engine.board().agents[0]
    );
    assert!(
        group_members(pgid).is_empty(),
        "the stopped shell is still alive"
    );
    assert!(
        gewu_multiverse::transcript::terminal_record(&run_dir).is_some(),
        "the normal stop left no durable terminal record"
    );
    assert!(
        !run_dir
            .join(gewu_multiverse::engine::session::PID_FILE)
            .exists(),
        "the identity handle survived the normal stop"
    );
    drop(engine);
    // Reopen: no leftover and Start works.
    let mut reopened = engine::open_with(options(&layout, &data_root)).expect("reopen");
    assert!(
        wait_ready(&mut reopened, Duration::from_secs(150)),
        "reopen Forgejo"
    );
    assert!(
        reopened.board().agents[0].leftover.is_none(),
        "a cleanly stopped run blocked the reopen: {:?}",
        reopened.board().agents[0].leftover
    );
    let first_id = run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the first run id")
        .to_owned();
    reopened
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start after a clean stop");
    assert!(
        wait_until(Duration::from_secs(60), || {
            reopened.tick();
            newest_run_dir(&layout)
                .and_then(|dir| {
                    dir.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .is_some_and(|id| id != first_id)
        }),
        "the replacement run was not prepared"
    );
    assert!(
        wait_until(Duration::from_secs(60), || {
            reopened.tick();
            newest_run_dir(&layout).is_some_and(|dir| {
                gewu_multiverse::transcript::read(&dir)
                    .map(|file| {
                        file.records.iter().any(|record| {
                            matches!(
                                record,
                                gewu_multiverse::transcript::Record::RunStarted { .. }
                            )
                        })
                    })
                    .unwrap_or(false)
            })
        }),
        "the replacement run never started"
    );
    let _ = reopened.send(EngineCommand::Stop {
        name: "agent-1".to_owned(),
    });
    for _ in 0..20 {
        reopened.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(reopened);
    if std::env::var_os("GEWU_LIVE_KEEP").is_none() {
        let _ = fs::remove_dir_all(&root);
    } else {
        eprintln!("kept live root {}", root.display());
    }
}

/// A real model-only run (no shell ever starts) stopped before it can write an
/// ending: the failed ending write must keep the session handles, the repaired
/// `Start` must write the ending and clear them, and a replacement run must start.
#[test]
#[ignore = "live: needs verified Forgejo bytes via GEWU_LIVE_FORGEJO"]
fn live_engine_model_only_stop_keeps_and_repairs_its_handles() {
    let root = scratch("engine-model-only");
    let data_root = root.join("data");
    let mut slow = StubReply::json(final_reply());
    slow.delay = Duration::from_secs(30);
    let stub = Stub::start(vec![slow, StubReply::json(final_reply())]);
    let layout = seed_with_instruction(
        &root,
        &data_root,
        &stub.url(),
        "modelonly: answer without running a shell.\n",
    );
    let mut engine = engine::open_with(options(&layout, &data_root)).expect("engine opens");
    assert!(
        wait_ready(&mut engine, Duration::from_secs(150)),
        "the real Forgejo did not become ready"
    );
    engine
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start");
    let first = wait_until(Duration::from_secs(30), || {
        engine.tick();
        matches!(
            engine.board().agents[0].session,
            SessionState::Running { .. }
        ) && newest_run_dir(&layout).is_some_and(|dir| {
            dir.join(gewu_multiverse::engine::session::PID_FILE)
                .is_file()
        })
    });
    assert!(first, "the model-only run never started");
    let first_dir = newest_run_dir(&layout).expect("the model-only run directory");
    let first_id = first_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("run id")
        .to_owned();
    assert!(
        !first_dir.join(run::SHELL_PGID_FILE).exists()
            && !first_dir.join(run::SHELL_CAPABILITY_FILE).exists(),
        "the model-only run unexpectedly created a shell identity"
    );
    // The ending cannot be written during the stop, but the directory stays
    // writable: the handles remain because this code keeps them, not because an
    // unlink happened to fail.
    fs::set_permissions(
        first_dir.join(gewu_multiverse::transcript::TRANSCRIPT_FILE),
        fs::Permissions::from_mode(0o400),
    )
    .expect("the ending record becomes unwritable");
    engine
        .send(EngineCommand::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("Stop");
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine.tick();
            engine.board().agents[0].leftover.is_some()
        }),
        "the failed ending write was not reported: {:?}",
        engine.board().agents[0]
    );
    assert!(
        gewu_multiverse::transcript::terminal_record(&first_dir).is_none(),
        "an ending was claimed while its record was unwritable"
    );
    assert!(
        first_dir
            .join(gewu_multiverse::engine::session::PID_FILE)
            .is_file()
            && first_dir.join(run::PGID_FILE).is_file(),
        "the session handles were removed before a durable ending"
    );
    // Repair and let Start perform the documented retry.
    fs::set_permissions(
        first_dir.join(gewu_multiverse::transcript::TRANSCRIPT_FILE),
        fs::Permissions::from_mode(0o600),
    )
    .expect("the ending record is repaired");
    engine
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start retries the ending");
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine.tick();
            gewu_multiverse::transcript::terminal_record(&first_dir).is_some()
                && !first_dir
                    .join(gewu_multiverse::engine::session::PID_FILE)
                    .exists()
                && !first_dir.join(run::PGID_FILE).exists()
        }),
        "the repaired retry did not write the ending and clear the handles: {:?}",
        engine.board().agents[0]
    );
    assert!(
        wait_until(Duration::from_secs(60), || {
            engine.tick();
            newest_run_dir(&layout)
                .and_then(|dir| {
                    dir.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .is_some_and(|id| id != first_id)
        }),
        "the replacement run after repair was not prepared"
    );
    assert!(
        wait_until(Duration::from_secs(60), || {
            engine.tick();
            newest_run_dir(&layout).is_some_and(|dir| {
                gewu_multiverse::transcript::read(&dir)
                    .map(|file| {
                        file.records.iter().any(|record| {
                            matches!(
                                record,
                                gewu_multiverse::transcript::Record::RunStarted { .. }
                            )
                        })
                    })
                    .unwrap_or(false)
            })
        }),
        "the replacement run after repair never started"
    );
    let _ = engine.send(EngineCommand::Stop {
        name: "agent-1".to_owned(),
    });
    for _ in 0..30 {
        engine.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(engine);
    let mut reopened = engine::open_with(options(&layout, &data_root)).expect("reopen");
    assert!(
        reopened.board().agents[0].leftover.is_none(),
        "the repaired model-only run still blocked the reopen: {:?}",
        reopened.board().agents[0].leftover
    );
    drop(reopened);
    if std::env::var_os("GEWU_LIVE_KEEP").is_none() {
        let _ = fs::remove_dir_all(&root);
    } else {
        eprintln!("kept live root {}", root.display());
    }
}

/// The helper process that owns the real engine until the parent kills it.
#[test]
#[ignore = "live helper: started by the live engine test"]
fn fx_r35_engine_helper() {
    let Ok(root) = std::env::var("GEWU_LIVE_HELPER_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let data_root = root.join("data");
    let layout = Layout::rooted_at(root.clone()).expect("layout");
    let mut engine = engine::open_with(options(&layout, &data_root)).expect("helper engine");
    assert!(
        wait_ready(&mut engine, Duration::from_secs(150)),
        "helper Forgejo"
    );
    engine
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("helper Start");
    assert!(
        wait_until(Duration::from_secs(60), || {
            engine.tick();
            matches!(
                engine.board().agents[0].session,
                SessionState::Running { .. }
            )
        }),
        "helper run never started"
    );
    fs::write(root.join("helper-running"), b"running").expect("marker");
    loop {
        engine.tick();
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn process_cmdline(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
        .unwrap_or_default()
}

fn task_forgejo_pids(data_root: &Path) -> Vec<u32> {
    let needle = data_root.to_string_lossy().to_string();
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        if process_cmdline(pid).contains(&needle) {
            pids.push(pid);
        }
    }
    pids
}

/// A product (engine) killed with `SIGKILL`: the reopen must reap the real sleeping
/// shell, record its ending and leave the agent startable.
#[test]
#[ignore = "live: needs verified Forgejo bytes via GEWU_LIVE_FORGEJO"]
fn live_engine_killed_product_reopen() {
    let root = scratch("engine-kill");
    let data_root = root.join("data");
    let stub = Stub::start(vec![
        StubReply::json(tool_call("sleep 300")),
        StubReply::json(final_reply()),
    ]);
    let layout = seed(&root, &data_root, &stub.url());
    let mut helper: Child = Command::new(std::env::current_exe().expect("test exe"))
        .args([
            "--ignored",
            "--exact",
            "fx_r35_engine_helper",
            "--nocapture",
        ])
        .env("GEWU_LIVE_HELPER_ROOT", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("helper engine");
    let helper_pid = helper.id();
    assert!(
        wait_until(Duration::from_secs(180), || {
            root.join("helper-running").is_file()
                && newest_run_dir(&layout).is_some_and(|dir| {
                    shell_pgid(&dir).is_some_and(|pgid| !group_members(pgid).is_empty())
                })
        }),
        "the helper's sleeping run never appeared"
    );
    let run_dir = newest_run_dir(&layout).expect("run dir");
    let pgid = shell_pgid(&run_dir).expect("shell pgid");
    assert!(
        !group_members(pgid).is_empty(),
        "the helper's shell is not alive"
    );
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(helper_pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = helper.wait();
    assert!(
        wait_until(Duration::from_secs(10), || {
            !group_members(pgid).is_empty()
        }),
        "the shell did not survive the product kill; the reopen cannot be proven"
    );
    let mut reopened = engine::open_with(options(&layout, &data_root)).expect("reopen after kill");
    assert!(
        wait_until(Duration::from_secs(20), || {
            reopened.tick();
            reopened.board().agents[0].leftover.is_none()
                && gewu_multiverse::transcript::terminal_record(&run_dir).is_some()
        }),
        "the reopen did not settle the killed run: {:?}",
        reopened.board().agents[0]
    );
    assert!(
        group_members(pgid).is_empty(),
        "the reopen left the real shell alive"
    );
    assert!(
        run_dir.join(run::STOPPED_FILE).is_file(),
        "the reopen wrote no run-stopped record"
    );
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir).expect("ending");
    assert!(
        format!("{terminal:?}").contains("stopped"),
        "the reopen ending is not stopped: {terminal:?}"
    );
    assert!(
        !run_dir
            .join(gewu_multiverse::engine::session::PID_FILE)
            .exists(),
        "the identity handle survived the reopen cleanup"
    );
    // Start works after the recovered ending.
    let _ = wait_ready(&mut reopened, Duration::from_secs(120));
    let first_id = run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the killed run id")
        .to_owned();
    reopened
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start after recovery");
    assert!(
        wait_until(Duration::from_secs(60), || {
            reopened.tick();
            newest_run_dir(&layout)
                .and_then(|dir| {
                    dir.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .is_some_and(|id| id != first_id)
        }),
        "Start after recovery prepared no replacement run"
    );
    assert!(
        wait_until(Duration::from_secs(60), || {
            reopened.tick();
            newest_run_dir(&layout).is_some_and(|dir| {
                gewu_multiverse::transcript::read(&dir)
                    .map(|file| {
                        file.records.iter().any(|record| {
                            matches!(
                                record,
                                gewu_multiverse::transcript::Record::RunStarted { .. }
                            )
                        })
                    })
                    .unwrap_or(false)
            })
        }),
        "the replacement run after recovery never started"
    );
    let _ = reopened.send(EngineCommand::Stop {
        name: "agent-1".to_owned(),
    });
    for _ in 0..20 {
        reopened.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(reopened);
    // Exact identity teardown: only processes whose command line names this test's
    // data root, checked after the engine's own shutdown.
    for pid in task_forgejo_pids(&data_root) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    if std::env::var_os("GEWU_LIVE_KEEP").is_none() {
        let _ = fs::remove_dir_all(&root);
    } else {
        eprintln!("kept live root {}", root.display());
    }
}

/// A real loopback run sequence proves the scheduling words: a quick failure waits
/// about 10 s; a following successful no-work run resets the streak and waits the
/// documented 20 s, not 40 s.
#[test]
#[ignore = "live: needs verified Forgejo bytes via GEWU_LIVE_FORGEJO"]
fn live_engine_no_work_resets_the_failure_wait() {
    let root = scratch("engine-no-work-live");
    let data_root = root.join("data");
    let no_work = serde_json::json!({
        "model": "live-fixture",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "There are no open issues assigned to me."}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
    })
    .to_string();
    let failure = serde_json::json!({"error": {"message": "fixture server error"}}).to_string();
    let stub = Stub::start(vec![
        StubReply::status(500, failure),
        StubReply::json(no_work.clone()),
        StubReply::json(no_work),
    ]);
    let layout = seed(&root, &data_root, &stub.url());
    let mut engine = engine::open_with(options(&layout, &data_root)).expect("engine opens");
    assert!(
        wait_ready(&mut engine, Duration::from_secs(150)),
        "the real Forgejo did not become ready"
    );
    engine
        .send(EngineCommand::Start {
            name: "agent-1".to_owned(),
        })
        .expect("Start");

    let mut run_dirs: Vec<PathBuf> = Vec::new();
    let mut starts: Vec<Instant> = Vec::new();
    let mut saw_no_work_wait = false;
    let deadline = Instant::now() + Duration::from_secs(130);
    while Instant::now() < deadline && starts.len() < 3 {
        engine.tick();
        let board = engine.board();
        if let SessionState::Restarting { last, .. } = &board.agents[0].session {
            if matches!(last, SessionEnd::NoWork { .. }) {
                saw_no_work_wait = true;
            }
        }
        if let Some(dir) = newest_run_dir(&layout) {
            if !run_dirs.iter().any(|seen| seen == &dir) {
                if let Ok(file) = gewu_multiverse::transcript::read(&dir) {
                    if file.records.iter().any(|record| {
                        matches!(
                            record,
                            gewu_multiverse::transcript::Record::RunStarted { .. }
                        )
                    }) {
                        run_dirs.push(dir.clone());
                        starts.push(Instant::now());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        starts.len() >= 3,
        "the loopback run sequence did not reach three runs: {starts:?}"
    );
    let first_wait = starts[1].duration_since(last_finished_for(&run_dirs, 0));
    assert!(
        first_wait >= Duration::from_secs(8) && first_wait <= Duration::from_secs(25),
        "the quick failure did not wait about 10 s: {first_wait:?}"
    );
    let second_wait = starts[2].duration_since(last_finished_for(&run_dirs, 1));
    assert!(
        second_wait >= Duration::from_secs(17) && second_wait <= Duration::from_secs(32),
        "the no-work success did not wait the documented 20 s: {second_wait:?}"
    );
    assert!(
        saw_no_work_wait,
        "the board never showed the no-work waiting state"
    );
    // The third wait must not be the 40 s the still-growing failure streak would ask
    // for after two failures; the second run reset it.
    assert!(
        second_wait < Duration::from_secs(35),
        "the no-work success used a stale failure streak: {second_wait:?}"
    );
    let _ = engine.send(EngineCommand::Stop {
        name: "agent-1".to_owned(),
    });
    for _ in 0..30 {
        engine.tick();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(engine);
    if std::env::var_os("GEWU_LIVE_KEEP").is_none() {
        let _ = fs::remove_dir_all(&root);
    } else {
        eprintln!("kept live root {}", root.display());
    }
}

fn last_finished_for(runs: &[PathBuf], index: usize) -> Instant {
    // The caller recorded starts at the moment each RunStarted was observed; the
    // terminal time is read from the run's mtime so the wait is measured on the
    // product's own records rather than the test's last loop pass.
    let dir = &runs[index];
    let metadata = fs::metadata(dir.join("transcript.jsonl")).expect("transcript metadata");
    let modified = metadata.modified().expect("transcript mtime");
    let elapsed = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Instant::now() - elapsed
}
