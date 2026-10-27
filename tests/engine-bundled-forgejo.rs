//! The bundled Forgejo and the engine's session loop, against real parts.
//!
//! The installer tests are ordinary tests: a loopback server serves a small file, and
//! the same hash check the 122 MB release goes through decides. The live test is
//! `#[ignore]`d and run explicitly with a real Forgejo binary named by
//! `GEWU_FORGEJO_BINARY`; it fails rather than passing when the binary is absent.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::bundled_forgejo;
use gewu_multiverse::credentials;
use gewu_multiverse::engine::session::{RealSpawner, SessionRequest, SessionSpawner};
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{Command, EngineApi, ForgejoState, Provisioning, SessionState};
use gewu_multiverse::forgejo::{DEFAULT_TIMEOUT, ForgejoClient};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::process_alive;
use gewu_multiverse::test_stub::{Stub, StubReply};
use sha2::{Digest, Sha256};

#[path = "../src/test_wait.rs"]
mod test_wait;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// A scratch root that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("gewu-engine-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("the scratch root is made");
        Self(root)
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

fn sha256(text: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn the_installer_refuses_a_wrong_hash_and_accepts_the_right_one() {
    let root = Scratch::new("installer");
    let body = b"a small stand-in for the release, served over loopback";
    let expected = sha256(body);
    let source = Stub::start(vec![StubReply::status(
        200,
        String::from_utf8_lossy(body).into_owned(),
    )]);
    let target = root
        .path()
        .join("wrong")
        .join("forgejo")
        .join(bundled_forgejo::ASSET);
    let error =
        bundled_forgejo::install_from_sources(&[source.url()], &target, &"0".repeat(64), |_| {})
            .expect_err("a wrong hash is refused");
    assert!(error.contains("SHA-256"), "{error}");
    assert!(!target.exists(), "a wrong hash kept the target");
    assert_eq!(
        fs::read_dir(target.parent().expect("parent"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "a wrong hash left a part file"
    );

    let target = root
        .path()
        .join("right")
        .join("forgejo")
        .join(bundled_forgejo::ASSET);
    let installed =
        bundled_forgejo::install_from_sources(&[source.url()], &target, &expected, |_| {})
            .expect("the right hash is installed");
    assert_eq!(installed, target);
    assert_eq!(
        bundled_forgejo::sha256_file(&target).expect("the target hashes"),
        expected
    );
}

#[test]
fn the_release_source_is_tried_before_the_official_one() {
    let sources = bundled_forgejo::download_sources();
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(sources[1], bundled_forgejo::OFFICIAL_URL);
    assert!(
        sources[0].starts_with("https://github.com/Callo42/gewu-multiverse/releases/download/v"),
        "{}",
        sources[0]
    );
    assert!(
        sources[0].ends_with(bundled_forgejo::ASSET),
        "{}",
        sources[0]
    );
    assert_eq!(
        bundled_forgejo::release_asset_url(),
        format!(
            "https://github.com/Callo42/gewu-multiverse/releases/download/v{}/{}",
            env!("CARGO_PKG_VERSION"),
            bundled_forgejo::ASSET
        )
    );
}

#[test]
fn a_wrong_first_source_falls_through_to_the_next_one() {
    let root = Scratch::new("installer-wrong-then-right");
    let body = b"the release, served by the second source";
    let expected = sha256(body);
    let wrong = Stub::start(vec![StubReply::status(200, "not the release")]);
    let right = Stub::start(vec![StubReply::status(
        200,
        String::from_utf8_lossy(body).into_owned(),
    )]);
    let target = root.path().join("forgejo").join(bundled_forgejo::ASSET);
    let installed = bundled_forgejo::install_from_sources(
        &[wrong.url(), right.url()],
        &target,
        &expected,
        |_| {},
    )
    .expect("the second source is accepted");
    assert_eq!(installed, target);
    assert_eq!(
        bundled_forgejo::sha256_file(&target).expect("the target hashes"),
        expected
    );
}

#[test]
fn a_missing_first_source_falls_through_to_the_next_one() {
    let root = Scratch::new("installer-missing-then-right");
    let body = b"the release, after an absent first answer";
    let expected = sha256(body);
    let missing = Stub::start(vec![StubReply::status(404, "no asset on this release")]);
    let right = Stub::start(vec![StubReply::status(
        200,
        String::from_utf8_lossy(body).into_owned(),
    )]);
    let target = root.path().join("forgejo").join(bundled_forgejo::ASSET);
    bundled_forgejo::install_from_sources(
        &[missing.url(), right.url()],
        &target,
        &expected,
        |_| {},
    )
    .expect("the second source is accepted");
    assert_eq!(
        bundled_forgejo::sha256_file(&target).expect("the target hashes"),
        expected
    );
}

#[test]
fn both_sources_failing_is_one_line_that_says_the_next_start_tries_again() {
    let root = Scratch::new("installer-both-fail");
    let first = Stub::start(vec![StubReply::status(404, "no asset")]);
    let second = Stub::start(vec![StubReply::status(200, "wrong bytes")]);
    let target = root.path().join("forgejo").join(bundled_forgejo::ASSET);
    let error = bundled_forgejo::install_from_sources(
        &[first.url(), second.url()],
        &target,
        &"0".repeat(64),
        |_| {},
    )
    .expect_err("the install is refused");
    assert_eq!(error.lines().count(), 1, "{error}");
    assert!(error.contains("next start tries again"), "{error}");
    assert!(!target.exists(), "a failed install kept the target");
    assert_eq!(
        fs::read_dir(target.parent().expect("parent"))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "a failed install left a part file"
    );
}

/// A process whose command line looks like the Forgejo server this product starts.
fn fake_forgejo_command(data_root: &Path) -> ProcessCommand {
    let paths = bundled_forgejo::InstancePaths::under(data_root);
    let binary = bundled_forgejo::binary_path(data_root);
    let mut command = ProcessCommand::new("/bin/bash");
    command
        .arg("-c")
        .arg(format!(
            "exec -a {} /bin/sh -c 'sleep 60' web --config {} --work-path {}",
            binary.display(),
            paths.app_ini.display(),
            paths.root.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
}

#[test]
fn a_left_over_forgejo_is_found_by_its_command_line_and_stopped() {
    let root = Scratch::new("stale-forgejo");
    let paths = bundled_forgejo::InstancePaths::under(root.path());
    fs::create_dir_all(&paths.root).expect("the instance root");
    let mut child = fake_forgejo_command(root.path())
        .spawn()
        .expect("the fake server runs");
    let pid = child.id();
    // The shell execs into the shaped command line; wait for that before the
    // recovery reads it, as a real start has long done.
    test_wait::wait_until("the fake Forgejo's shaped command line", || {
        bundled_forgejo::command_line(pid).is_some_and(|parts| {
            parts.iter().any(|part| part == "web")
                && parts
                    .iter()
                    .any(|part| part == &paths.app_ini.display().to_string())
        })
    });
    fs::write(paths.root.join(bundled_forgejo::PID_FILE), pid.to_string()).expect("the pid record");
    let stopped = bundled_forgejo::recover_stale_server(root.path(), test_wait::LIVENESS_TIMEOUT)
        .expect("the recovery runs");
    assert_eq!(stopped, Some(pid), "the recorded server was not found");
    let _ = child.wait();
    assert!(!process_alive(pid), "the left-over server is still running");
    assert!(
        !paths.root.join(bundled_forgejo::PID_FILE).exists(),
        "the pid record was kept"
    );
}

#[test]
fn a_reused_pid_with_another_command_line_is_never_killed() {
    let root = Scratch::new("stale-forgejo-wrong");
    let paths = bundled_forgejo::InstancePaths::under(root.path());
    fs::create_dir_all(&paths.root).expect("the instance root");
    let mut child = ProcessCommand::new("/bin/sh")
        .arg("-c")
        .arg("sleep 60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("the unrelated child runs");
    let pid = child.id();
    fs::write(paths.root.join(bundled_forgejo::PID_FILE), pid.to_string()).expect("the pid record");
    let stopped = bundled_forgejo::recover_stale_server(root.path(), Duration::from_secs(1))
        .expect("the recovery runs");
    assert_eq!(stopped, None, "an unrelated process was believed");
    assert!(process_alive(pid), "an unrelated process was killed");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_left_over_session_is_stopped_before_a_new_engine_starts() {
    let root = Scratch::new("stale-session");
    let layout =
        Layout::rooted_at(root.path().join("config")).expect("the test layout is absolute");
    let run_dir = layout.agent("agent-1").expect("paths").run_dir("run-1");
    fs::create_dir_all(&run_dir).expect("the run directory");
    // The modern ownership record: an inherited anonymous capability. The fake
    // session is not the product supervisor executable, so its argv alone must not
    // own the group; it must hold the capability like a real run child.
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let fd = nix::sys::memfd::memfd_create(
        "gewu-stale-session-capability",
        nix::sys::memfd::MFdFlags::empty(),
    )
    .expect("memfd");
    let mut capability = fs::File::from(fd);
    let token = [0x3c_u8; 16];
    capability.write_all(&token).expect("token");
    capability.seek(SeekFrom::Start(0)).expect("seek");
    let metadata = capability.metadata().expect("metadata");
    fs::write(
        run_dir.join("shell.capability"),
        format!(
            "{} {} {}\n",
            metadata.dev(),
            metadata.ino(),
            token
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
    )
    .expect("capability record");
    let mut child = ProcessCommand::new("/bin/sh")
        .arg("-c")
        .arg("sleep 60")
        .arg("--run-agent")
        .arg(&run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("the fake session runs");
    drop(capability);
    let pid = child.id();
    test_wait::wait_until("the fake session's shaped command line", || {
        bundled_forgejo::command_line(pid).is_some_and(|parts| {
            parts.iter().any(|part| part == "--run-agent")
                && parts
                    .iter()
                    .any(|part| part == &run_dir.display().to_string())
        })
    });
    fs::write(
        run_dir.join(gewu_multiverse::engine::session::PID_FILE),
        pid.to_string(),
    )
    .expect("the pid record");
    let options = EngineOptions {
        layout: layout.clone(),
        data_root: root.path().join("data"),
        product_binary: PathBuf::from("/bin/true"),
        session_timeout: Duration::from_secs(60),
        stop_grace: Duration::from_secs(2),
        ready_timeout: Duration::from_secs(10),
        auto_start_forgejo: false,
        confinement: gewu_multiverse::confinement::Method::None,
    };
    let engine = engine::open_with(options).expect("the engine opens past the recovery");
    let _ = child.wait();
    assert!(
        !process_alive(pid),
        "the left-over session is still running"
    );
    assert!(
        !run_dir
            .join(gewu_multiverse::engine::session::PID_FILE)
            .exists(),
        "the session's pid record was kept"
    );
    drop(engine);
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> StubReply {
    StubReply::status(
        200,
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
}

fn final_reply(text: &str) -> StubReply {
    StubReply::status(
        200,
        serde_json::json!({
            "model": "lab-chat-1",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": text}
            }]
        })
        .to_string(),
    )
}

fn wait_until(what: &str, _product_ready_allowance: Duration, check: impl FnMut() -> bool) {
    test_wait::wait_until(what, check);
}

/// Tick the engine while waiting; the board alone does not advance its work. The
/// second argument is the explicit allowance for the external Forgejo start, kept
/// at the call sites; the wait itself uses the suite's liveness bound.
fn wait_engine(
    engine: &mut gewu_multiverse::engine::Engine,
    what: &str,
    _product_ready_allowance: Duration,
    mut check: impl FnMut(&mut gewu_multiverse::engine::Engine) -> bool,
) {
    test_wait::wait_until(what, || {
        engine.tick();
        check(engine)
    });
}

/// These engine-path cases predate the native programs. A fresh settings file
/// now defaults to Pi, so state the legacy built-in program explicitly instead of
/// relying on the old implicit default. This keeps the fixture testing the engine
/// path it was written for.
fn choose_builtin(engine: &mut gewu_multiverse::engine::Engine, names: &[&str]) {
    for name in names {
        engine
            .send(Command::SetAgentProgram {
                name: (*name).to_owned(),
                program: "builtin".to_owned(),
                protocol: None,
            })
            .expect("the built-in program is selectable");
    }
    wait_engine(
        engine,
        "the built-in program",
        Duration::from_secs(30),
        |engine| {
            let board = engine.board();
            names.iter().all(|name| {
                board
                    .agents
                    .iter()
                    .any(|agent| agent.name == *name && agent.program == "builtin")
            })
        },
    );
}

/// The live run: install, prepare, start, provision, two agents, one instructed
/// session that comments, a second session by itself, Stop, RemoveAgent with a 401,
/// and shutdown with every pid gone.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo -- --ignored --nocapture"]
fn the_bundled_engine_runs_a_started_agent_until_stopped() {
    let binary = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(binary.is_file(), "{} is not a file", binary.display());
    assert_eq!(
        bundled_forgejo::sha256_file(&binary).expect("the binary hashes"),
        bundled_forgejo::SHA256,
        "the staged Forgejo binary is not the pinned one"
    );
    let root = Scratch::new("live");
    let layout =
        Layout::rooted_at(root.path().join("config")).expect("the test layout is absolute");
    let data_root = root.path().join("data");
    let product = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    let mut seen_pids: Vec<u32> = Vec::new();

    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the bundled Forgejo to run and provision",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let board = engine.board();
    let url = board.forgejo.url.clone();
    eprintln!(
        "live: Forgejo at {url} with {} repositories",
        board.forgejo.repositories
    );
    // A new installation uses the fixed login, not the computer account name; the
    // account really works (the engine provisioned the organisation with it).
    assert_eq!(board.forgejo.admin_login, "owner");
    assert!(!board.forgejo.admin_login_kept);

    // `p` resets the administrator password through the administration API and shows the new
    // one; nothing is kept on disk.
    engine
        .send(Command::RevealAdminPassword)
        .expect("the password reset starts");
    let mut password = None;
    wait_engine(
        &mut engine,
        "the reset administrator password",
        Duration::from_secs(60),
        |engine| {
            if let Some(shown) = engine.board().admin_password {
                password = Some(shown);
                true
            } else {
                false
            }
        },
    );
    let password = password.expect("the new password");
    assert_eq!(
        password.chars().count(),
        69,
        "the reset password is not the full value"
    );
    let paths = bundled_forgejo::InstancePaths::under(&data_root);
    assert!(
        !paths.root.join("secrets/admin-password").exists(),
        "the administrator password is still on disk"
    );
    eprintln!("live: administrator password reset through the administration API; no file kept");

    // Prove the lifecycle: a dead child leaves a reason in the view and is restarted
    // with back-off, on the same data and port.
    let old_forgejo = *engine.live_pids().first().expect("the Forgejo has a pid");
    seen_pids.push(old_forgejo);
    kill(Pid::from_raw(old_forgejo as i32), Signal::SIGKILL).expect("the Forgejo is killed");
    let mut reason = String::new();
    wait_engine(
        &mut engine,
        "the Forgejo death to be visible",
        Duration::from_secs(30),
        |engine| match &engine.board().forgejo.state {
            ForgejoState::Failed { reason: why } => {
                reason = why.clone();
                true
            }
            _ => false,
        },
    );
    assert!(
        !reason.trim().is_empty(),
        "the view gave no reason for the death"
    );
    eprintln!("live: Forgejo died; the view says: {reason}");
    wait_engine(
        &mut engine,
        "the Forgejo to restart",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let new_forgejo = *engine
        .live_pids()
        .first()
        .expect("the restarted Forgejo has a pid");
    assert_ne!(new_forgejo, old_forgejo, "the same pid came back");
    for pid in engine.live_pids() {
        if !seen_pids.contains(&pid) {
            seen_pids.push(pid);
        }
    }
    eprintln!("live: Forgejo restarted as pid {new_forgejo}");

    engine
        .send(Command::CreateAgents { count: 2 })
        .expect("agents");
    wait_engine(
        &mut engine,
        "both agent accounts",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 2
                && board
                    .agents
                    .iter()
                    .all(|agent| agent.provisioning == Provisioning::Ready)
        },
    );
    choose_builtin(&mut engine, &["agent-1", "agent-2"]);
    let board = engine.board();
    assert_eq!(board.agents[0].login, "gewu-agent-1");
    assert_eq!(board.agents[1].login, "gewu-agent-2");

    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("the credentials file reads")
        .expect("the administrator token is recorded");
    let admin_client =
        ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("an administrator client");
    let team = bundled_forgejo::agents_team(&admin_client).expect("the agents' team");
    assert_eq!(
        team.permission, "write",
        "Forgejo reports the agents' team permission as {:?}",
        team.permission
    );
    let members = admin_client
        .team_members(team.id)
        .expect("the team's members");
    assert!(
        members.iter().any(|member| member.login == "gewu-agent-1")
            && members.iter().any(|member| member.login == "gewu-agent-2"),
        "the agent accounts did not join team agents"
    );
    let issue = admin_client
        .create_issue(
            "team",
            "workspace",
            "Work for the agent",
            "Please leave a comment here.",
        )
        .expect("the issue is opened");
    eprintln!("live: opened team/workspace#{}", issue.number);

    let mut comment_call = tool_call(
        "comment-1",
        "forgejo_comment",
        serde_json::json!({
            "repository": "team/workspace",
            "number": issue.number,
            "body": "agent-1 was here",
        }),
    );
    // Keep the session alive long enough for the test to record its pid.
    comment_call.delay = Duration::from_secs(2);
    let mut done = final_reply("the comment is written");
    done.delay = Duration::from_millis(500);
    let model = Stub::start(vec![comment_call, done]);
    let instruction = engine
        .instruction_path("agent-1")
        .expect("agent-1 has an instruction path");
    fs::write(
        &instruction,
        format!(
            "Comment `agent-1 was here` on team/workspace#{} and then finish.\n",
            issue.number
        ),
    )
    .expect("the instruction is written");
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "lab-chat-1".to_owned(),
            key: Some("gw-engine-live-key".to_owned()),
        })
        .expect("the endpoint is saved");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 starts");

    wait_engine(
        &mut engine,
        "the first session to start",
        Duration::from_secs(120),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            engine.board().agents[0].sessions_since_start >= 1
        },
    );
    wait_until("the comment on the issue", Duration::from_secs(120), || {
        for pid in engine.live_pids() {
            if !seen_pids.contains(&pid) {
                seen_pids.push(pid);
            }
        }
        admin_client
            .issue_comments("team", "workspace", issue.number, 20)
            .map(|comments| {
                comments
                    .iter()
                    .any(|comment| comment.author.as_deref() == Some("gewu-agent-1"))
            })
            .unwrap_or(false)
    });
    let comments = admin_client
        .issue_comments("team", "workspace", issue.number, 20)
        .expect("the comments read");
    assert!(
        comments.iter().any(|comment| {
            comment.author.as_deref() == Some("gewu-agent-1")
                && comment.body.contains("agent-1 was here")
        }),
        "the agent's comment is not on the issue"
    );
    eprintln!("live: agent-1 commented on the issue");
    wait_engine(
        &mut engine,
        "the activity line",
        Duration::from_secs(10),
        |engine| {
            engine.board().agents[0]
                .activity
                .as_deref()
                .is_some_and(|activity| activity.contains("commented on #"))
        },
    );
    eprintln!("live: activity={:?}", engine.board().agents[0].activity);

    wait_engine(
        &mut engine,
        "the second session to start by itself",
        Duration::from_secs(120),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            engine.board().agents[0].sessions_since_start >= 2
        },
    );
    for pid in engine.live_pids() {
        if !seen_pids.contains(&pid) {
            seen_pids.push(pid);
        }
    }
    eprintln!(
        "live: sessions_since_start={} recent={:?}",
        engine.board().agents[0].sessions_since_start,
        engine.board().agents[0].recent
    );
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop");
    wait_engine(
        &mut engine,
        "agent-1 to be idle",
        Duration::from_secs(30),
        |engine| matches!(engine.board().agents[0].session, SessionState::Idle),
    );

    // A known password first proves the web sign-in works; after removal the same
    // sign-in must be refused, not merely its token.
    let probe_password = "Gw-Probe-2026-abcdef";
    admin_client
        .set_user_password("gewu-agent-1", probe_password)
        .expect("the agent password is set for the sign-in proof");
    let (signed_in_before, page_before) = web_sign_in(&url, "gewu-agent-1", probe_password);
    assert!(
        signed_in_before,
        "the agent could not sign in before removal: {page_before}"
    );
    eprintln!("live: the agent's web sign-in works before removal");

    // The token that is about to be revoked, kept only to prove it answers 401 after.
    let old_token = credentials::forgejo_agent_token(&layout.credentials_file(), "agent-1")
        .expect("agent-1's token is recorded");
    engine
        .send(Command::RemoveAgent {
            name: "agent-1".to_owned(),
        })
        .expect("remove agent-1");
    wait_engine(
        &mut engine,
        "agent-1 to be removed",
        Duration::from_secs(60),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].name == "agent-2"
        },
    );
    let status = token_status(&url, old_token.expose());
    assert_eq!(status, 401, "the removed agent's old token still answers");
    let removed = admin_client
        .user("gewu-agent-1")
        .expect("the account is read")
        .expect("the Forgejo account was deleted with the agent");
    assert!(
        !removed.active,
        "the removed agent's account is still active"
    );
    let members = admin_client
        .team_members(team.id)
        .expect("the team's members");
    assert!(
        !members.iter().any(|member| member.login == "gewu-agent-1"),
        "the removed agent is still in team agents"
    );
    let (signed_in_after, page_after) = web_sign_in(&url, "gewu-agent-1", probe_password);
    assert!(
        !signed_in_after,
        "the removed agent still signs in on the web: {page_after}"
    );
    assert!(
        page_after.to_lowercase().contains("suspended")
            || page_after.to_lowercase().contains("prohibit")
            || page_after.to_lowercase().contains("sign in"),
        "the refusal page says nothing about the deactivation: {page_after}"
    );
    eprintln!(
        "live: removed agent-1; its old token answers 401, its web sign-in is refused, its account stays, team agents no longer lists it"
    );

    for pid in engine.live_pids() {
        if !seen_pids.contains(&pid) {
            seen_pids.push(pid);
        }
    }
    engine.shutdown(Duration::from_secs(10));
    engine.tick();
    let after = engine.live_pids();
    assert!(
        after.is_empty(),
        "the engine still knows live pids: {after:?}"
    );
    for pid in &seen_pids {
        assert!(
            !process_alive(*pid),
            "pid {pid} is still alive after shutdown"
        );
    }
    assert_eq!(
        token_status(&url, "gw-never-a-token"),
        0,
        "the server still answers after shutdown"
    );
    let record = serde_json::json!({
        "url": url,
        "issue": issue.number,
        "comments_before_stop": comments.len(),
        "sessions_since_start": 2,
        "old_token_status": status,
        "signed_in_before": signed_in_before,
        "signed_in_after": signed_in_after,
        "pids": seen_pids,
    });
    if let Ok(path) = std::env::var("GEWU_ENGINE_LIVE_RECORD") {
        fs::write(path, serde_json::to_string_pretty(&record).expect("record"))
            .expect("record written");
    }
    eprintln!("live: shutdown; every recorded pid is gone");
}

/// The first run after a clean reopen has the Forgejo context and the six tools. The
/// agent is left started, the Forgejo is stopped, and the program is opened again;
/// Forgejo reaches `Running` before its provisioning job has returned the
/// organisation's repositories, which is the window after a
/// `kill -9` recovery.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo -- --ignored --nocapture"]
fn the_first_run_after_a_reopen_has_the_forgejo_tools() {
    let binary = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(binary.is_file(), "{} is not a file", binary.display());
    assert_eq!(
        bundled_forgejo::sha256_file(&binary).expect("the binary hashes"),
        bundled_forgejo::SHA256,
        "the staged Forgejo binary is not the pinned one"
    );
    let root = Scratch::new("live-reopen");
    let layout =
        Layout::rooted_at(root.path().join("config")).expect("the test layout is absolute");
    let data_root = root.path().join("data");
    let product = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let model = Stub::start(vec![final_reply("the first recovered run is done")]);

    // First visit: make the Forgejo and one ready agent, leave the agent started,
    // then close cleanly. No tick after Start, so this visit starts no run.
    let mut first = engine::open_with(options.clone()).expect("the first engine opens");
    first.send(Command::AcknowledgeTerms).expect("terms");
    first.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut first,
        "the bundled Forgejo to run and provision",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    first
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(
        &mut first,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    choose_builtin(&mut first, &["agent-1"]);
    let instruction = first
        .instruction_path("agent-1")
        .expect("agent-1 has an instruction path");
    fs::write(
        &instruction,
        "Say that the recovered run is done and finish.\n",
    )
    .expect("the instruction is written");
    first
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "lab-chat-1".to_owned(),
            key: Some("gw-reopen-live-key".to_owned()),
        })
        .expect("the endpoint is saved");
    first
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 is left started");
    assert_eq!(
        first.board().agents[0].sessions_since_start,
        0,
        "a run started before the reopen"
    );
    let mut seen: Vec<u32> = first.live_pids();
    first.shutdown(Duration::from_secs(10));
    drop(first);
    assert!(
        seen.iter().all(|pid| !process_alive(*pid)),
        "the first visit left a process: {seen:?}"
    );

    // Second visit: the agent is already started and ready, and Forgejo has to come
    // up again. The session must not start until provisioning has returned the
    // repositories, so the first run carries forgejo.json and the six tools.
    let mut second = engine::open_with(options).expect("the reopened engine opens");
    second.send(Command::StartForgejo).expect("start Forgejo");
    let (run_dir, log): (PathBuf, String) =
        test_wait::wait_for("the first recovered run's tool line", || {
            second.tick();
            fs::read_dir(layout.state_root().join("agent-1"))
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let run_dir = entry.path();
                    let log = fs::read_to_string(run_dir.join("run.log")).ok()?;
                    log.contains("forgejo tools:").then_some((run_dir, log))
                })
                .next()
        });
    for pid in second.live_pids() {
        if !seen.contains(&pid) {
            seen.push(pid);
        }
    }
    assert!(
        log.contains("forgejo tools: on as `gewu-agent-1`"),
        "the first recovered run is shell-only:\n{log}"
    );
    assert!(
        log.contains("on team/workspace"),
        "the first recovered run has no repository context:\n{log}"
    );
    let context = gewu_multiverse::run::read_forgejo_context(&run_dir)
        .expect("the first recovered run has forgejo.json");
    assert_eq!(
        context
            .repositories
            .iter()
            .map(|repository| repository.full_name.as_str())
            .collect::<Vec<_>>(),
        ["team/workspace"]
    );
    test_wait::wait_until("the first recovered run's model request", || {
        second.tick();
        !model.requests().is_empty()
    });
    let body = &model.requests()[0].body;
    for name in gewu_multiverse::forgejo_tools::TOOL_NAMES {
        assert!(
            body.contains(name),
            "the first recovered run's model request does not declare {name}"
        );
    }
    eprintln!(
        "live: the first run after the reopen has forgejo.json and six tools; log says {}",
        log.lines()
            .find(|line| line.contains("forgejo tools:"))
            .unwrap_or_default()
    );

    second
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop agent-1");
    wait_engine(
        &mut second,
        "agent-1 to be idle",
        Duration::from_secs(30),
        |engine| matches!(engine.board().agents[0].session, SessionState::Idle),
    );
    for pid in second.live_pids() {
        if !seen.contains(&pid) {
            seen.push(pid);
        }
    }
    second.shutdown(Duration::from_secs(10));
    second.tick();
    assert!(
        second.live_pids().is_empty(),
        "the engine still knows live pids: {:?}",
        second.live_pids()
    );
    for pid in &seen {
        assert!(
            !process_alive(*pid),
            "pid {pid} is still alive after shutdown"
        );
    }
    if let Ok(path) = std::env::var("GEWU_ENGINE_RECOVERY_RECORD") {
        let record = serde_json::json!({
            "run_dir": run_dir,
            "forgejo_context": context.repositories,
            "tools": gewu_multiverse::forgejo_tools::TOOL_NAMES,
            "pids": seen,
        });
        fs::write(path, serde_json::to_string_pretty(&record).expect("record"))
            .expect("record written");
    }
}

/// A relative `--config-dir` or `--state-dir` is made absolute when the layout is
/// built, so the child started in the agent's workspace still reads the plan and
/// writes the run log where the engine means them to go.
#[test]
fn a_relative_configuration_root_writes_the_first_runs_plan_and_log() {
    use std::os::unix::fs::PermissionsExt;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let relative =
        PathBuf::from("target").join(format!("relative-first-run-{}-{stamp}", std::process::id()));
    let layout = Layout::rooted_at(relative).expect("the relative root is accepted");
    assert!(layout.root().is_absolute(), "the root stayed relative");
    assert!(
        layout.state_root().is_absolute(),
        "the state root stayed relative"
    );
    let agent = layout
        .ensure_agent("agent-1")
        .expect("the agent is prepared");
    fs::write(agent.instruction(), "say hello and finish\n").expect("the instruction");
    gewu_multiverse::credentials::CredentialsFile::prepare(&layout.credentials_file())
        .expect("the credentials file");
    credentials::record_secret(
        &layout.credentials_file(),
        None,
        "model_key",
        "gw-relative-key",
    )
    .expect("the model key");

    let model = Stub::start(vec![final_reply("hello")]);
    let request = SessionRequest {
        agent: "agent-1".to_owned(),
        run_id: "run-relative-0001".to_owned(),
        run_dir: agent.run_dir("run-relative-0001"),
        state_root: layout.state_root().to_path_buf(),
        workspace: agent.workspace().to_path_buf(),
        instruction: agent.instruction().to_path_buf(),
        skills_dir: agent.skills_dir().to_path_buf(),
        endpoint: model.url(),
        model: "lab-chat-1".to_owned(),
        max_turns: 4,
        timeout_secs: 60,
        credentials: layout.credentials_file(),
        login: "gewu-agent-1".to_owned(),
        email: "gewu-agent-1@agents.invalid".to_owned(),
        forgejo_url: "http://127.0.0.1:1".to_owned(),
        native: None,
        repositories: Vec::new(),
        confinement: gewu_multiverse::confinement::Method::None,
        hide: Vec::new(),
    };
    let spawner = RealSpawner::new(PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse")));
    let prepared = spawner.prepare(&request).expect("the run is prepared");
    assert!(
        prepared.run_dir.is_absolute(),
        "the run directory stayed relative"
    );
    let mut running = spawner.spawn(&prepared).expect("the run starts");
    let status = test_wait::wait_for("the first run to finish", || running.try_status());
    assert!(status.success(), "the first run failed: {status:?}");
    assert!(
        prepared.run_dir.join("plan.json").is_file(),
        "the first run has no plan.json"
    );
    assert!(
        prepared.run_dir.join("run.log").is_file(),
        "the first run has no run.log"
    );
    let log = fs::read_to_string(prepared.run_dir.join("run.log")).expect("the log reads");
    assert!(
        log.contains("agent process exiting with code 0"),
        "the run log does not record its ending:\n{log}"
    );
    for dir in [
        layout.state_root(),
        agent.state_dir(),
        prepared.run_dir.as_path(),
    ] {
        let mode = fs::metadata(dir)
            .expect("the directory can be described")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{dir:?} is not private");
    }
    let _ = fs::remove_dir_all(layout.root());
}

/// One request with a token, for the revocation proof. A refused connection is `0`.
/// Sign in through Forgejo's own web form, following redirects; returns whether the
/// final page is the signed-in one and the page's own words.
fn web_sign_in(url: &str, user: &str, password: &str) -> (bool, String) {
    let output = ProcessCommand::new("curl")
        .args([
            "-sS",
            "-L",
            "--max-time",
            "15",
            "-w",
            "\nFINAL=%{url_effective}",
            "--data-urlencode",
            &format!("user_name={user}"),
            "--data-urlencode",
            &format!("password={password}"),
            &format!("{url}/user/login"),
        ])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let final_url = text
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("FINAL="))
        .unwrap_or_default()
        .to_owned();
    let logged_in = !final_url.is_empty() && !final_url.contains("/user/login");
    (logged_in, text)
}

/// Standard base64, for HTTP Basic auth. Kept tiny so the test has no new dependency.
fn base64(text: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in text.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// One API call authenticated with a login and password, as a person would.
fn api_as_user(url: &str, user: &str, password: &str) -> (u16, String) {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let header = format!("Basic {}", base64(format!("{user}:{password}").as_bytes()));
    match agent
        .get(format!("{url}/api/v1/user"))
        .header("Authorization", header)
        .call()
    {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.into_body().read_to_string().unwrap_or_default();
            (status, body)
        }
        Err(_) => (0, String::new()),
    }
}

fn token_status(url: &str, token: &str) -> u16 {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    match agent
        .get(format!("{url}/api/v1/user"))
        .header("Authorization", format!("token {token}"))
        .call()
    {
        Ok(response) => response.status().as_u16(),
        Err(_) => 0,
    }
}

/// Whether the instance's port still accepts a connection.
fn listener_on(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

/// The live quit-at-many-points run: open the engine, start the real Forgejo, and
/// shut down at 30 points from immediately after `StartForgejo` to just after
/// `Running`. After every cycle, every recorded pid must be gone and the instance
/// port must refuse.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo quitting_at_every_point -- --ignored --nocapture"]
fn quitting_at_every_point_of_a_forgejo_start_leaves_nothing() {
    let binary = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(binary.is_file(), "{} is not a file", binary.display());
    assert_eq!(
        bundled_forgejo::sha256_file(&binary).expect("the binary hashes"),
        bundled_forgejo::SHA256,
        "the staged Forgejo binary is not the pinned one"
    );
    let root = Scratch::new("live-quit-start");
    let layout =
        Layout::rooted_at(root.path().join("config")).expect("the test layout is absolute");
    let data_root = root.path().join("data");
    let options = || {
        let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
        options.auto_start_forgejo = false;
        options.ready_timeout = Duration::from_secs(240);
        options.stop_grace = Duration::from_secs(5);
        options
    };
    let pid_file = bundled_forgejo::InstancePaths::under(&data_root)
        .root
        .join(bundled_forgejo::PID_FILE);
    let app_ini = bundled_forgejo::InstancePaths::under(&data_root).app_ini;
    let read_pid = || {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    };
    let instance_port = || {
        fs::read_to_string(&app_ini)
            .ok()
            .and_then(|text| bundled_forgejo::port_from_config(&text))
    };

    // A warm-up start measures how long this machine takes. It also writes the
    // instance, so every measured cycle starts from the same prepared state.
    let mut engine = engine::open_with(options()).expect("the engine opens");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    let began = Instant::now();
    let mut starting: Option<u128> = None;
    let ready = test_wait::wait_for("the warm-up start to reach running", || {
        engine.tick();
        let state = engine.board().forgejo.state;
        if matches!(state, ForgejoState::Starting) && starting.is_none() {
            starting = Some(began.elapsed().as_millis());
        }
        matches!(state, ForgejoState::Running).then(|| began.elapsed())
    });
    let ready_ms = ready.as_millis() as u64;
    let port = instance_port().expect("the warm-up wrote a port");
    engine.shutdown(Duration::from_secs(2));
    drop(engine);
    assert!(!listener_on(port), "the warm-up left the port listening");
    eprintln!("live quit-start: warm-up reached running in {ready_ms} ms on port {port}");

    // The warm-up also says when the first child appeared (`Starting`). Thirty quits:
    // eight by elapsed time early in the start, sixteen at the first `Starting` frame
    // plus a short suffix, and six at the first `● running` frame plus a suffix.
    let starting_ms = starting.map(|value| value as u64).unwrap_or(ready_ms / 2);
    let mut plan: Vec<(&str, u64)> = Vec::new();
    for index in 0..8 {
        plan.push(("elapsed", starting_ms * index / 7));
    }
    for index in 0..16 {
        plan.push(("starting", index * 10));
    }
    for suffix in [0u64, 50, 100, 150, 250, 400] {
        plan.push(("running", suffix));
    }
    assert_eq!(plan.len(), 30);

    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let mut records = Vec::new();
    for (number, (kind, target)) in plan.iter().enumerate() {
        let mut engine = engine::open_with(options()).expect("the engine opens");
        engine.send(Command::StartForgejo).expect("start Forgejo");
        let began = Instant::now();
        let mut seen: Vec<u32> = Vec::new();
        let mut starting_hit: Option<u128> = None;
        let mut running_hit: Option<u128> = None;
        let state_at_quit = loop {
            engine.tick();
            for pid in engine.live_pids() {
                if !seen.contains(&pid) {
                    seen.push(pid);
                }
            }
            if let Some(pid) = read_pid()
                && !seen.contains(&pid)
            {
                seen.push(pid);
            }
            let state = engine.board().forgejo.state.clone();
            let elapsed = began.elapsed().as_millis();
            if matches!(state, ForgejoState::Starting) && starting_hit.is_none() && !seen.is_empty()
            {
                starting_hit = Some(elapsed);
            }
            if matches!(state, ForgejoState::Running) && running_hit.is_none() {
                running_hit = Some(elapsed);
                // A fast start can move from `Starting` to `Running` between two
                // ticks; the `Running` frame is then the first start the test saw,
                // and the plan's `starting` suffix is measured from it (test harness
                // only; review correction 9, NB-B).
                if starting_hit.is_none() {
                    starting_hit = Some(elapsed);
                }
            }
            let reached = match *kind {
                "starting" => starting_hit.is_some_and(|hit| elapsed >= hit + *target as u128),
                "running" => running_hit.is_some_and(|hit| elapsed >= hit + *target as u128),
                _ => elapsed >= *target as u128,
            };
            if reached {
                break state;
            }
            assert!(
                elapsed < 60_000,
                "cycle {number}: the start never reached {kind}+{target} ms"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let quit_after_ms = began.elapsed().as_millis();
        let shutdown_began = Instant::now();
        // The product quits with a two-second grace; the test uses the same bound so
        // a server that is slow to stop gracefully is forced and reaped.
        engine.shutdown(Duration::from_secs(2));
        let shutdown_ms = shutdown_began.elapsed().as_millis();
        let mut pids_alive_after: Vec<u32> = Vec::new();
        for pid in &seen {
            if !test_wait::wait_until_or(|| !process_alive(*pid)) {
                pids_alive_after.push(*pid);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        let listening_after = listener_on(port);
        let engine_pids = engine.live_pids();
        assert!(
            pids_alive_after.is_empty(),
            "cycle {number}: pids still alive after shutdown: {pids_alive_after:?} (seen {seen:?})"
        );
        assert!(
            engine_pids.is_empty(),
            "cycle {number}: the engine still records {engine_pids:?}"
        );
        assert!(
            !listening_after,
            "cycle {number}: port {port} still accepts after shutdown"
        );
        assert!(
            shutdown_ms < 5_000,
            "cycle {number}: shutdown took {shutdown_ms} ms"
        );
        let record = serde_json::json!({
            "cycle": number + 1,
            "target_ms": target,
            "quit_after_ms": quit_after_ms,
            "target_kind": kind,
            "target_offset_ms": target,
            "starting_hit_ms": starting_hit,
            "running_hit_ms": running_hit,
            "state_at_quit": format!("{state_at_quit:?}"),
            "pids": seen,
            "shutdown_ms": shutdown_ms,
            "pids_alive_after": pids_alive_after,
            "listener_after": listening_after,
        });
        eprintln!(
            "live quit-start cycle {:02}: {}+{} ms quit={} ms state={:?} pids={:?} shutdown={} ms listener={}",
            number + 1,
            kind,
            target,
            quit_after_ms,
            state_at_quit,
            record["pids"],
            shutdown_ms,
            listening_after
        );
        records.push(record);
    }
    let report = serde_json::json!({
        "binary_sha256": bundled_forgejo::SHA256,
        "ready_ms": ready_ms,
        "starting_ms": starting_ms,
        "port": port,
        "started_at_unix_ms": started_at,
        "cycles": records,
    });
    if let Ok(path) = std::env::var("GEWU_QUIT_CYCLES_RECORD") {
        fs::write(
            path,
            serde_json::to_string_pretty(&report).expect("the record is JSON"),
        )
        .expect("the cycle record is written");
    }
    eprintln!("live quit-start: 30 cycles, every pid gone and no listener");
}

/// Open a live engine on a scratch root, with the pinned binary checked.
fn live_open(root: &Path, auto_start: bool) -> (gewu_multiverse::engine::Engine, Layout, PathBuf) {
    let binary = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(binary.is_file(), "{} is not a file", binary.display());
    assert_eq!(
        bundled_forgejo::sha256_file(&binary).expect("the binary hashes"),
        bundled_forgejo::SHA256,
        "the staged Forgejo binary is not the pinned one"
    );
    let layout = Layout::rooted_at(root.join("config")).expect("the test layout is absolute");
    let data_root = root.join("data");
    let mut options = EngineOptions::new(layout.clone(), &data_root).expect("options");
    options.product_binary = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    options.auto_start_forgejo = auto_start;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let engine = engine::open_with(options).expect("the engine opens");
    (engine, layout, data_root)
}

/// The newest run log of one agent, if a run has written one.
fn newest_run_log(layout: &Layout, agent: &str) -> Option<String> {
    let paths = layout.agent(agent).ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(paths.state_dir()).ok()?.flatten() {
        let path = entry.path().join("run.log");
        if let Ok(metadata) = fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                if newest.as_ref().is_none_or(|(at, _)| modified > *at) {
                    newest = Some((modified, path));
                }
            }
        }
    }
    newest.and_then(|(_, path)| fs::read_to_string(path).ok())
}

/// A confined agent proves three things against the real Forgejo in one run: the
/// product's secret paths and the Forgejo CLI are unreadable inside the run; `git
/// push` authenticates through the per-run askpass file (B1); and the agent opens a
/// pull request as itself.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo a_confined_agent_pushes -- --ignored --nocapture"]
fn a_confined_agent_pushes_a_branch_and_opens_a_pull_request() {
    let root = Scratch::new("live-confined-push");
    let (mut engine, layout, data_root) = live_open(root.path(), false);
    let mut seen_pids: Vec<u32> = Vec::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the bundled Forgejo to run and provision",
        Duration::from_secs(240),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let url = engine.board().forgejo.url.clone();
    assert!(
        engine.board().confined,
        "the host is unconfined; this test needs a confinement"
    );
    eprintln!("live: confined at {url}");

    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("the credentials file reads")
        .expect("the administrator token is recorded");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue("team", "workspace", "Confined push", "Push a branch here.")
        .expect("the issue is opened");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("one agent");
    wait_engine(
        &mut engine,
        "the agent account",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 1 && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    choose_builtin(&mut engine, &["agent-1"]);
    fs::write(
        engine
            .instruction_path("agent-1")
            .expect("the instruction path"),
        "Push a branch and open a pull request.\n",
    )
    .expect("the instruction is written");

    let instance = bundled_forgejo::InstancePaths::under(&data_root);
    let forgejo_binary = bundled_forgejo::binary_path(&data_root);
    let real_home = std::env::var("HOME").unwrap_or_else(|_| "/none".to_owned());
    let attack = format!(
        "set +e\n\
         for p in '{config}/credentials.toml' '{config}/multiverse.toml' '{state}' '{app}' '{db}' '{secrets}' '{binary}'; do\n\
           if cat \"$p\" >/dev/null 2>&1; then printf 'secret_read=yes %s\\n' \"$p\"; else printf 'secret_read=no %s\\n' \"$p\"; fi\n\
         done\n\
         if '{binary}' admin user list --config '{app}' --work-path '{instance}' >/dev/null 2>&1; then echo 'forgejo_admin=yes'; else echo 'forgejo_admin=no'; fi\n\
         if cat '{home}/.gitconfig' >/dev/null 2>&1 || cat '{home}/.ssh/id_ed25519' >/dev/null 2>&1; then echo 'home_read=yes'; else echo 'home_read=no'; fi\n\
         printf 'model_key_env=%s\\n' \"${{GEWU_MODEL_KEY:-unset}}\"\n\
         printf 'openai_env=%s\\n' \"${{OPENAI_API_KEY:-unset}}\"\n",
        config = layout.root().display(),
        state = layout.state_root().display(),
        app = instance.app_ini.display(),
        db = instance.data_dir.join("forgejo.db").display(),
        secrets = instance.root.join("secrets").display(),
        binary = forgejo_binary.display(),
        instance = instance.root.display(),
        home = real_home,
    );
    let push = "cd workspace && git checkout -b fx-r31-confined && \
                printf 'a confined push\\n' > confined.txt && \
                git add confined.txt && \
                git -c user.email=gewu-agent-1@agents.invalid -c user.name=gewu-agent-1 commit -m 'confined push' -q && \
                git push -u origin fx-r31-confined && echo push_ok"
        .to_owned();
    let model = Stub::start(vec![
        tool_call("attack", "shell", serde_json::json!({ "command": attack })),
        tool_call("push", "shell", serde_json::json!({ "command": push })),
        tool_call(
            "pr",
            "forgejo_open_pull_request",
            serde_json::json!({
                "repository": "team/workspace",
                "title": "A confined push",
                "body": format!("The confined agent pushed this for issue #{}.", issue.number),
                "branch": "fx-r31-confined",
                "base": "main",
            }),
        ),
        final_reply("the branch is pushed and the pull request is open"),
    ]);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "lab-chat-1".to_owned(),
            key: Some("gw-confined-live-key".to_owned()),
        })
        .expect("the endpoint is saved");
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("agent-1 starts");

    wait_engine(
        &mut engine,
        "the confined session to start",
        Duration::from_secs(90),
        |engine| {
            let board = engine.board();
            board.agents.first().is_some_and(|agent| {
                agent.sessions_since_start >= 1
                    || matches!(agent.session, SessionState::Running { .. })
            })
        },
    );
    wait_engine(
        &mut engine,
        "the pull request",
        Duration::from_secs(180),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            admin_client
                .pull_requests("team", "workspace")
                .map(|pulls| pulls.iter().any(|pull| pull.head == "fx-r31-confined"))
                .unwrap_or(false)
        },
    );
    let log = loop {
        if let Some(log) = newest_run_log(&layout, "agent-1") {
            if log.contains("push_ok") {
                break log;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    eprintln!(
        "live: newest run log has {} bytes; secret_read=no x{}, forgejo_admin=no={}, home_read=no={}, model_key_env=unset={}",
        log.len(),
        log.matches("secret_read=no").count(),
        log.contains("forgejo_admin=no"),
        log.contains("home_read=no"),
        log.contains("model_key_env=unset"),
    );
    eprintln!(
        "live: parent-exported OPENAI_API_KEY absent in the run: {}",
        log.contains("openai_env=unset")
    );
    let no_reads = log.matches("secret_read=no").count();
    assert!(
        no_reads >= 7,
        "the run read {no_reads} of the seven secret paths:\n{log}"
    );
    assert!(
        log.contains("forgejo_admin=no"),
        "the run reached the Forgejo CLI:\n{log}"
    );
    assert!(
        log.contains("home_read=no"),
        "the run read the person's home:\n{log}"
    );
    assert!(
        log.contains("model_key_env=unset"),
        "the model key was in the run environment:\n{log}"
    );
    assert!(
        log.contains("openai_env=unset"),
        "a parent-exported key was inherited by the run:\n{log}"
    );
    assert!(log.contains("push_ok"), "the confined push failed:\n{log}");
    assert!(
        !log.contains("unable to read askpass") && !log.contains("could not read Password"),
        "git could not authenticate in the confined run:\n{log}"
    );
    let pulls = admin_client
        .pull_requests("team", "workspace")
        .expect("the pull requests");
    let pull = pulls
        .iter()
        .find(|pull| pull.head == "fx-r31-confined")
        .expect("the pull request is there");
    assert_eq!(pull.base, "main");
    eprintln!(
        "live: a confined agent pushed fx-r31-confined and opened pull request #{}",
        pull.number
    );

    // A repository created and deleted by the administrator between two polls is an
    // event, not a silent disappearance.
    admin_client
        .ensure_organisation_repository("team", "scratch-event")
        .expect("the scratch repository is created");
    wait_engine(
        &mut engine,
        "the created repository to be seen",
        Duration::from_secs(30),
        |engine| {
            engine
                .board()
                .forgejo
                .repository_names
                .iter()
                .any(|name| name == "team/scratch-event")
        },
    );
    admin_client
        .delete_repository("team", "scratch-event")
        .expect("the scratch repository is deleted");
    wait_engine(
        &mut engine,
        "the deletion event",
        Duration::from_secs(30),
        |engine| {
            engine
                .board()
                .forgejo
                .events
                .iter()
                .any(|event| event.contains("team/scratch-event") && event.contains("deleted"))
        },
    );
    eprintln!("live: a deleted repository shows as an event on the board");

    // The agent's own token cannot delete a repository, and the repository stays.
    let agent_token = credentials::forgejo_agent_token(&layout.credentials_file(), "agent-1")
        .expect("agent-1's token is recorded");
    let agent_client =
        ForgejoClient::new(&url, agent_token, DEFAULT_TIMEOUT).expect("an agent client");
    let refused = agent_client.delete_repository("team", "workspace");
    assert!(
        refused.is_err(),
        "an agent token deleted a repository: {refused:?}"
    );
    assert!(
        admin_client
            .repository("team", "workspace")
            .expect("the repository reads")
            .is_some(),
        "the repository disappeared after the refusal"
    );
    eprintln!("live: the agent's own token was refused a repository deletion");

    let record = serde_json::json!({
        "confinement": engine.board().confined,
        "pull": pull.number,
        "events": engine.board().forgejo.events,
        "run_log": log,
    });
    if let Ok(path) = std::env::var("GEWU_LIVE_EVIDENCE") {
        fs::write(path, serde_json::to_string_pretty(&record).expect("record"))
            .expect("evidence written");
    }
    for pid in engine.live_pids() {
        if !seen_pids.contains(&pid) {
            seen_pids.push(pid);
        }
    }
    engine.shutdown(Duration::from_secs(10));
    engine.tick();
    for pid in &seen_pids {
        assert!(!process_alive(*pid), "pid {pid} is still alive");
    }
}

/// A tiny loopback model for the two-agent load: the first turn of every run asks for
/// one comment on the issue, the turn after a tool result ends the run, and every
/// twenty-fifth request asks for a comment in a repository that is not there so a
/// failed tool call is exercised too.
struct LoadModel {
    addr: SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LoadModel {
    fn start(issue: u64) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("the load model binds");
        let addr = listener.local_addr().expect("its address");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut served = 0usize;
            for stream in listener.incoming() {
                if stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut raw = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            raw.extend_from_slice(&buffer[..read]);
                            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&raw[..end + 4]).into_owned();
                                let length = headers
                                    .lines()
                                    .find_map(|line| {
                                        line.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|value| value.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if raw.len() >= end + 4 + length {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let body = String::from_utf8_lossy(&raw);
                let after_tool = body.contains("\"role\":\"tool\"");
                served += 1;
                let reply = if after_tool {
                    serde_json::json!({
                        "model": "lab-chat-1",
                        "choices": [{
                            "index": 0,
                            "finish_reason": "stop",
                            "message": {"role": "assistant", "content": "load turn finished"}
                        }]
                    })
                } else if served % 7 == 0 {
                    serde_json::json!({
                        "model": "lab-chat-1",
                        "choices": [{
                            "index": 0,
                            "finish_reason": "tool_calls",
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": format!("slow-{served}"),
                                    "type": "function",
                                    "function": {
                                        "name": "shell",
                                        "arguments": serde_json::json!({"command": "sleep 2"}).to_string()
                                    }
                                }]
                            }
                        }]
                    })
                } else if served % 25 == 0 {
                    serde_json::json!({
                        "model": "lab-chat-1",
                        "choices": [{
                            "index": 0,
                            "finish_reason": "tool_calls",
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": format!("fail-{served}"),
                                    "type": "function",
                                    "function": {
                                        "name": "forgejo_comment",
                                        "arguments": serde_json::json!({
                                            "repository": "team/not-there",
                                            "number": 1,
                                            "body": "this call must fail"
                                        }).to_string()
                                    }
                                }]
                            }
                        }]
                    })
                } else {
                    serde_json::json!({
                        "model": "lab-chat-1",
                        "choices": [{
                            "index": 0,
                            "finish_reason": "tool_calls",
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": format!("comment-{served}"),
                                    "type": "function",
                                    "function": {
                                        "name": "forgejo_comment",
                                        "arguments": serde_json::json!({
                                            "repository": "team/workspace",
                                            "number": issue,
                                            "body": format!("load comment {served}")
                                        }).to_string()
                                    }
                                }]
                            }
                        }]
                    })
                };
                let payload = reply.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/v1/chat/completions", self.addr)
    }
}

impl Drop for LoadModel {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Rewrite an `app.ini` the way v0.1.30 left it: 500 ms, no WAL, no `[i18n]`/`[time]`.
fn downgrade_app_ini(text: &str) -> String {
    let mut out = String::new();
    let mut skip_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            skip_section = matches!(section, "i18n" | "time");
            if skip_section {
                continue;
            }
        }
        if skip_section {
            continue;
        }
        if trimmed.starts_with("SQLITE_JOURNAL_MODE") {
            continue;
        }
        out.push_str(&line.replace("SQLITE_TIMEOUT = 5000", "SQLITE_TIMEOUT = 500"));
        out.push('\n');
    }
    out
}

/// An upgraded installation keeps its account; the administrator token is minted
/// after the server starts, and `p` resets the password through the API. The new
/// value must work for a real API sign-in, and no file may keep it.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo upgraded_password -- --ignored --nocapture"]
fn the_password_resets_on_an_upgraded_v0130_instance() {
    let root = Scratch::new("live-upgrade-password");
    // First open: a complete instance, as a fresh build makes it, then stop it.
    let (mut engine, layout, data_root) = live_open(root.path(), false);
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the first Forgejo",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    engine.shutdown(Duration::from_secs(10));
    drop(engine);

    let instance = bundled_forgejo::InstancePaths::under(&data_root);
    let fresh = fs::read_to_string(&instance.app_ini).expect("the fresh app.ini");
    fs::write(&instance.app_ini, downgrade_app_ini(&fresh)).expect("the v0.1.30-shaped app.ini");
    // An earlier installation has no administrator token minted by this build's
    // setup path. Remove it so the second open must mint one after the server is up.
    let credentials = layout.credentials_file();
    let text = fs::read_to_string(&credentials).expect("the credentials file");
    let filtered: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("forgejo_admin_token"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&credentials, filtered).expect("the credentials without the admin token");
    let mut permissions = fs::metadata(&credentials)
        .expect("the credentials metadata")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o600);
    fs::set_permissions(&credentials, permissions).expect("the credentials mode");
    assert!(
        credentials::forgejo_admin_token(&credentials)
            .expect("the credentials read")
            .is_none(),
        "the test did not remove the setup token"
    );

    // Second open: v0.1.30 shape, token minted after the start.
    let (mut engine, layout, _data_root) = live_open(root.path(), false);
    let mut seen_pids: Vec<u32> = Vec::new();
    engine
        .send(Command::StartForgejo)
        .expect("start the upgraded Forgejo");
    wait_engine(
        &mut engine,
        "the upgraded Forgejo",
        Duration::from_secs(240),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    assert!(
        credentials::forgejo_admin_token(&layout.credentials_file())
            .expect("the credentials read")
            .is_some(),
        "the upgraded install did not mint an administrator token after start"
    );
    let url = engine.board().forgejo.url.clone();
    engine
        .send(Command::RevealAdminPassword)
        .expect("the password reset starts");
    let mut password = None;
    wait_engine(
        &mut engine,
        "the reset administrator password",
        Duration::from_secs(60),
        |engine| {
            if let Some(shown) = engine.board().admin_password {
                password = Some(shown);
                true
            } else {
                false
            }
        },
    );
    let password = password.expect("the new password");
    assert_eq!(password.chars().count(), 69, "the whole value is not shown");
    let (status, body) = api_as_user(&url, "owner", &password);
    assert_eq!(
        status, 200,
        "the new password is refused by the API: {body}"
    );
    assert!(
        body.contains("\"login\":\"owner\""),
        "the API sign-in is not the administrator: {body}"
    );
    assert!(
        !bundled_forgejo::InstancePaths::under(&data_root)
            .root
            .join("secrets/admin-password")
            .exists(),
        "the administrator password is on disk"
    );
    eprintln!(
        "live: v0.1.30-shaped install minted its administrator token after start; p reset the password and the API accepted it"
    );

    engine.shutdown(Duration::from_secs(10));
    for pid in seen_pids {
        assert!(
            !process_alive(pid),
            "the Forgejo or a helper (pid {pid}) is still alive"
        );
    }
}

/// Two confined agents write through the real Forgejo for two minutes, against a data
/// directory shaped the way v0.1.30 left it: 500 ms, no WAL, no locale pins. The new
/// build must bring the pins forward before Forgejo starts (B2/N8), and the load must
/// produce no `database is locked`, no 5xx and no silent hang.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo two_agents_load -- --ignored --nocapture"]
fn two_agents_load_a_v0130_shaped_data_directory_for_two_minutes() {
    let root = Scratch::new("live-load");
    // First open: let the build make a complete instance, then stop it and put the
    // configuration back to the shape an earlier version wrote.
    let (mut engine, _layout, data_root) = live_open(root.path(), false);
    let mut seen_pids: Vec<u32> = Vec::new();
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the first Forgejo",
        Duration::from_secs(240),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    engine.shutdown(Duration::from_secs(10));
    drop(engine);
    let instance = bundled_forgejo::InstancePaths::under(&data_root);
    let fresh = fs::read_to_string(&instance.app_ini).expect("the fresh app.ini");
    let old = downgrade_app_ini(&fresh);
    fs::write(&instance.app_ini, &old).expect("the old-shaped app.ini");
    assert!(old.contains("SQLITE_TIMEOUT = 500"));
    assert!(!old.contains("SQLITE_JOURNAL_MODE"));
    assert!(!old.contains("[i18n]") && !old.contains("[time]"));
    // Put the database itself back to the non-WAL mode too.
    let status = ProcessCommand::new("python3")
        .arg("-c")
        .arg("import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('PRAGMA wal_checkpoint(TRUNCATE)'); print(c.execute('PRAGMA journal_mode=DELETE').fetchone()[0]); c.close()")
        .arg(instance.data_dir.join("forgejo.db"))
        .stdout(Stdio::null())
        .status()
        .expect("python3 runs");
    assert!(status.success());

    // Second open: the new build must upgrade the pins before it starts Forgejo.
    let (mut engine, layout, _data_root) = live_open(root.path(), false);
    engine
        .send(Command::StartForgejo)
        .expect("start Forgejo again");
    wait_engine(
        &mut engine,
        "the upgraded Forgejo",
        Duration::from_secs(240),
        |engine| {
            for pid in engine.live_pids() {
                if !seen_pids.contains(&pid) {
                    seen_pids.push(pid);
                }
            }
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running) && board.forgejo.repositories >= 1
        },
    );
    let upgraded = fs::read_to_string(&instance.app_ini).expect("the upgraded app.ini");
    for wanted in [
        "SQLITE_TIMEOUT = 5000",
        "SQLITE_JOURNAL_MODE = WAL",
        "[i18n]",
        "LANGS = en-US",
        "DEFAULT_UI_LOCATION = UTC",
    ] {
        assert!(
            upgraded.contains(wanted),
            "the upgrade did not write {wanted:?}:\n{upgraded}"
        );
    }
    assert!(
        upgraded.contains("HTTP_PORT"),
        "the upgrade dropped the port:\n{upgraded}"
    );
    let journal = ProcessCommand::new("python3")
        .arg("-c")
        .arg("import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); print(c.execute('PRAGMA journal_mode').fetchone()[0]); c.close()")
        .arg(instance.data_dir.join("forgejo.db"))
        .output()
        .expect("python3 runs");
    assert_eq!(
        String::from_utf8_lossy(&journal.stdout).trim(),
        "wal",
        "SQLite is not in WAL mode"
    );
    eprintln!(
        "live: a v0.1.30-shaped app.ini and database were brought to WAL/5000 before Forgejo started"
    );

    let url = engine.board().forgejo.url.clone();
    let admin = credentials::forgejo_admin_token(&layout.credentials_file())
        .expect("the credentials file reads")
        .expect("the administrator token is recorded");
    let admin_client = ForgejoClient::new(&url, admin, DEFAULT_TIMEOUT).expect("admin client");
    let issue = admin_client
        .create_issue("team", "workspace", "Load", "Two agents write here.")
        .expect("the issue is opened");
    engine
        .send(Command::CreateAgents { count: 2 })
        .expect("two agents");
    wait_engine(
        &mut engine,
        "both agent accounts",
        Duration::from_secs(120),
        |engine| {
            let board = engine.board();
            board.agents.len() == 2
                && board
                    .agents
                    .iter()
                    .all(|agent| agent.provisioning == Provisioning::Ready)
        },
    );
    choose_builtin(&mut engine, &["agent-1", "agent-2"]);
    for name in ["agent-1", "agent-2"] {
        fs::write(
            engine.instruction_path(name).expect("the instruction path"),
            "Leave one comment on the issue, then finish.\n",
        )
        .expect("the instruction is written");
    }
    let model = LoadModel::start(issue.number);
    engine
        .send(Command::SetEndpoint {
            url: model.url(),
            model: "lab-chat-1".to_owned(),
            key: Some("gw-load-live-key".to_owned()),
        })
        .expect("the endpoint is saved");
    for name in ["agent-1", "agent-2"] {
        engine
            .send(Command::Start {
                name: name.to_owned(),
            })
            .expect("the agent starts");
    }

    let started = Instant::now();
    let deadline = started + Duration::from_secs(120);
    let mut saw_tool = [false; 2];
    let mut saw_restart = [false; 2];
    let mut saw_failure = false;
    let mut longest_tool = 0u64;
    while Instant::now() < deadline {
        engine.tick();
        let board = engine.board();
        for pid in engine.live_pids() {
            if !seen_pids.contains(&pid) {
                seen_pids.push(pid);
            }
        }
        for (index, agent) in board.agents.iter().enumerate().take(2) {
            if agent.current_tool.is_some() {
                saw_tool[index] = true;
                longest_tool = longest_tool.max(agent.current_tool_seconds);
            }
            if matches!(agent.session, SessionState::Restarting { .. }) {
                saw_restart[index] = true;
            }
            if agent
                .last_failure
                .as_deref()
                // U6: a refused Forgejo write's card row says `write refused:` and
                // no longer repeats the tool's internal name.
                .is_some_and(|failure| failure.contains("write refused"))
            {
                saw_failure = true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let board = engine.board();
    eprintln!(
        "live: 120 s load: sessions {:?}, recent {:?}, longest tool {:?}s",
        board
            .agents
            .iter()
            .map(|agent| agent.sessions_since_start)
            .collect::<Vec<_>>(),
        board
            .agents
            .iter()
            .map(|agent| agent.recent.len())
            .collect::<Vec<_>>(),
        longest_tool
    );
    assert!(
        board
            .agents
            .iter()
            .all(|agent| agent.sessions_since_start >= 3),
        "a run did not keep starting: {board:?}"
    );
    assert!(
        saw_tool.iter().all(|seen| *seen),
        "no tool in flight was seen"
    );
    assert!(
        saw_restart.iter().all(|seen| *seen),
        "no five-second idle was seen"
    );
    assert!(
        saw_failure,
        "the deliberately failed comment never reached the card"
    );
    let comments = admin_client
        .issue_comments("team", "workspace", issue.number, 20)
        .expect("the comments read")
        .len();
    assert!(comments >= 2, "the two agents wrote {comments} comments");
    // The Forgejo log must not carry the lock error the finding was about, and no 5xx
    // may have answered during the load.
    let log = fs::read_to_string(instance.log_dir.join("forgejo.log")).expect("the Forgejo log");
    assert!(
        !log.contains("database is locked"),
        "the Forgejo log has `database is locked`"
    );
    assert!(
        !log.contains(" 500 "),
        "the Forgejo log has a 500 during the load"
    );
    eprintln!("live: 120 s, {comments} comments, 0 `database is locked`, 0 500");

    let record = serde_json::json!({
        "sessions": board.agents.iter().map(|agent| agent.sessions_since_start).collect::<Vec<_>>(),
        "comments": comments,
        "saw_tool": saw_tool,
        "saw_restart": saw_restart,
        "saw_failure": saw_failure,
        "longest_tool_seconds": longest_tool,
        "app_ini": upgraded,
        "journal_mode": String::from_utf8_lossy(&journal.stdout).trim().to_owned(),
    });
    if let Ok(path) = std::env::var("GEWU_LIVE_EVIDENCE") {
        fs::write(path, serde_json::to_string_pretty(&record).expect("record"))
            .expect("evidence written");
    }

    for name in ["agent-1", "agent-2"] {
        engine
            .send(Command::Stop {
                name: name.to_owned(),
            })
            .expect("stop");
    }
    wait_engine(
        &mut engine,
        "both agents idle",
        Duration::from_secs(30),
        |engine| {
            engine
                .board()
                .agents
                .iter()
                .all(|agent| matches!(agent.session, SessionState::Idle))
        },
    );
    for pid in engine.live_pids() {
        if !seen_pids.contains(&pid) {
            seen_pids.push(pid);
        }
    }
    engine.shutdown(Duration::from_secs(10));
    engine.tick();
    for pid in &seen_pids {
        assert!(!process_alive(*pid), "pid {pid} is still alive");
    }
}

/// U10: a real bundled page carries the product footer but no developer timings.
#[test]
#[ignore = "live Forgejo: run with GEWU_FORGEJO_BINARY=/path/to/forgejo cargo test --test engine-bundled-forgejo the_page_footer_is_not_a_debug_build -- --ignored --nocapture"]
fn the_page_footer_is_not_a_debug_build() {
    let binary = PathBuf::from(std::env::var("GEWU_FORGEJO_BINARY").unwrap_or_else(|_| {
        panic!("GEWU_FORGEJO_BINARY is not set; this live test verifies nothing without it")
    }));
    assert!(binary.is_file(), "{} is not a file", binary.display());
    let root = Scratch::new("footer");
    let layout =
        Layout::rooted_at(root.path().join("config")).expect("the test layout is absolute");
    let data_root = root.path().join("data");
    let product = PathBuf::from(env!("CARGO_BIN_EXE_gewu-multiverse"));
    let mut options = EngineOptions::new(layout, &data_root).expect("options");
    options.product_binary = product;
    options.auto_start_forgejo = false;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(5);
    let mut engine = engine::open_with(options).expect("the engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine.send(Command::StartForgejo).expect("start Forgejo");
    wait_engine(
        &mut engine,
        "the bundled Forgejo to run",
        Duration::from_secs(240),
        |engine| matches!(engine.board().forgejo.state, ForgejoState::Running),
    );
    let url = engine.board().forgejo.url.clone();
    let body = ureq::get(format!("{url}/user/login"))
        .call()
        .expect("the sign-in page answers")
        .into_body()
        .read_to_string()
        .expect("the page body reads");
    assert!(body.contains("Powered by Forgejo"), "no Forgejo footer");
    assert!(
        !body.contains("Template:") && !body.contains("Page: "),
        "the page footer still carries developer timings"
    );
    engine.shutdown(Duration::from_secs(5));
}
