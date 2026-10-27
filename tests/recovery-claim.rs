//! Recovery naming for a run whose supervisor was hard-killed.
//!
//! A hard kill delivers `SIGTERM` to the `--run-agent` supervisor through
//! `PR_SET_PDEATHSIG`, but the native child it started can outlive it and keeps
//! its own durable `native.identity`. The supervisor's `session.pid` record is
//! then dead while the child is alive in the same process group. Recovery must
//! still name that agent and run, and stop the child, rather than stopping at the
//! first (dead) record.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::engine;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run;

/// One run left by a hard kill: a live native child in a group whose recorded
/// supervisor is already gone.
struct HardKilledRun {
    run_dir: PathBuf,
    native: Option<Child>,
    native_pid: u32,
    native_pgid: u32,
    native_start: u64,
    boot: String,
}

impl HardKilledRun {
    /// Build the durable shape a hard kill leaves. The caller owns the layout;
    /// the child is killed on drop.
    fn new(layout: &Layout, agent: &str, run_id: &str) -> Self {
        let run_dir = layout.state_root().join(agent).join(run_id);
        fs::create_dir_all(&run_dir).expect("the run directory");

        // The supervisor leads its own group, exactly as the engine's
        // `--run-agent` spawn does. The native child joins that group.
        let mut supervisor = Command::new("/bin/sleep")
            .arg("120")
            .process_group(0)
            .spawn()
            .expect("the supervisor starts");
        let supervisor_pid = supervisor.id();
        let native = Command::new("/bin/sleep")
            .arg("120")
            .process_group(supervisor_pid as i32)
            .spawn()
            .expect("the native child starts");
        let native_pid = native.id();

        // The child's group is set between fork and exec; read it from the kernel
        // rather than assuming the parent observed the setpgid already.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (native_pgid, native_start) = loop {
            let (state, group, start) = stat_fields(native_pid).expect("the native child's stat");
            assert_ne!(state, 'Z', "the native child ended before the fixture");
            if group == supervisor_pid {
                break (group, start);
            }
            assert!(
                Instant::now() < deadline,
                "the native child never joined group {supervisor_pid}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("the boot id")
            .trim()
            .to_owned();

        // What the engine writes at spawn: the supervisor's pid/pgid. What the
        // native supervisor writes once the child exists: its own positive
        // identity.
        fs::write(run_dir.join("session.pid"), supervisor_pid.to_string()).expect("session.pid");
        fs::write(run_dir.join("session.pgid"), supervisor_pid.to_string()).expect("session.pgid");
        fs::write(
            run_dir.join(run::NATIVE_IDENTITY_FILE),
            format!("{native_pid} {native_pgid} {boot} {native_start}\n"),
        )
        .expect("native.identity");

        supervisor.kill().expect("the hard kill");
        supervisor.wait().expect("the supervisor is reaped");
        assert!(
            !run::process_alive(supervisor_pid),
            "the supervisor record must be dead for this fixture"
        );

        Self {
            run_dir,
            native: Some(native),
            native_pid,
            native_pgid,
            native_start,
            boot,
        }
    }

    fn alive(&mut self) -> bool {
        self.native
            .as_mut()
            .expect("the native child is owned")
            .try_wait()
            .expect("the native child's state")
            .is_none()
    }

    fn wait_gone(&mut self) {
        let mut native = self.native.take().expect("the native child is owned");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if native
                .try_wait()
                .expect("the native child's state")
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the native child survived the stop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !run::process_alive(self.native_pid),
            "the native child's pid is still alive"
        );
    }

    fn kill_native(&mut self) {
        let mut native = self.native.take().expect("the native child is owned");
        native.kill().expect("the native kill");
        native.wait().expect("the native child is reaped");
    }
}

impl Drop for HardKilledRun {
    fn drop(&mut self) {
        if let Some(mut child) = self.native.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A scratch installation holding one hard-killed run.
struct Fixture {
    root: PathBuf,
    layout: Layout,
    live: HardKilledRun,
}

impl Fixture {
    fn hard_killed(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "gewu-recovery-claim-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("the scratch root is made");
        let layout = Layout::rooted_at(root.join("config")).expect("the layout is absolute");
        layout.ensure_root().expect("the configuration root");
        let live = HardKilledRun::new(&layout, "agent-1", "run-1");
        Self { root, layout, live }
    }

    fn run_dir(&self) -> &Path {
        &self.live.run_dir
    }

    fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }

    fn claim(&self) -> Option<engine::RecoveryClaim> {
        engine::stale_recovery_claim(&self.layout, &self.data_root())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stat_fields(pid: u32) -> Option<(char, u32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let group = fields.next()?.parse().ok()?;
    for _ in 6..=21 {
        fields.next()?;
    }
    let start = fields.next()?.parse().ok()?;
    Some((state, group, start))
}

fn assert_names_agent_and_run(claim: &engine::RecoveryClaim, agent: &str, run_id: &str) {
    assert_eq!(claim.agent.as_deref(), Some(agent), "{claim:?}");
    assert_eq!(claim.run_id.as_deref(), Some(run_id), "{claim:?}");
    let message = claim.message();
    assert!(message.contains(agent), "{message}");
    assert!(message.contains(run_id), "{message}");
    assert!(!message.contains("Forgejo"), "{message}");
}

#[test]
fn a_hard_kill_with_a_live_native_child_is_named_and_stopped() {
    let mut fixture = Fixture::hard_killed("live-native");
    assert!(
        run::recorded_run_is_live(fixture.run_dir()),
        "the live native identity was hidden by the dead supervisor record"
    );
    assert!(
        engine::stale_recovery_pending(&fixture.layout, &fixture.data_root()),
        "the opening screen was not told about the live native run"
    );
    let claim = fixture.claim().expect("the live native run is claimed");
    assert_names_agent_and_run(&claim, "agent-1", "run-1");

    let stopped = run::recover_ending(fixture.run_dir(), Duration::from_millis(250))
        .expect("recovery stops the verified native group");
    assert!(stopped, "recovery found nothing to stop");
    fixture.live.wait_gone();
}

#[test]
fn a_dead_only_run_claims_nothing_and_does_not_hide_a_live_one() {
    let mut fixture = Fixture::hard_killed("dead-only");
    // The native child is gone too: only dead records remain. A second run was
    // hard-killed the same way but its native child is still alive, so the
    // reclaim must skip the dead record and name the live run.
    fixture.live.kill_native();
    assert!(!run::process_alive(fixture.live.native_pid));
    assert!(!run::recorded_run_is_live(fixture.run_dir()));
    let control = HardKilledRun::new(&fixture.layout, "agent-2", "run-live");
    assert!(control.run_dir.is_dir());
    assert!(run::recorded_run_is_live(&control.run_dir));

    assert!(
        engine::stale_recovery_pending(&fixture.layout, &fixture.data_root()),
        "the live control run was not seen"
    );
    let claim = fixture.claim().expect("the live control run is claimed");
    assert_names_agent_and_run(&claim, "agent-2", "run-live");
    assert!(
        !run::recover_ending(fixture.run_dir(), Duration::from_millis(50))
            .expect("a dead run is cleaned without a signal"),
        "a dead run reported a stopped process"
    );
}

#[test]
fn a_live_process_that_is_not_the_runs_is_not_signalled() {
    let mut fixture = Fixture::hard_killed("unverifiable");
    // The same pid and group, but the boot/start identity no longer matches the
    // process the record names: this is not proof that the process is the run's.
    fs::write(
        fixture.run_dir().join(run::NATIVE_IDENTITY_FILE),
        format!(
            "{} {} {} {}\n",
            fixture.live.native_pid,
            fixture.live.native_pgid,
            fixture.live.boot,
            fixture.live.native_start.saturating_add(1)
        ),
    )
    .expect("native.identity");
    assert!(
        !run::recorded_run_is_live(fixture.run_dir()),
        "an unverifiable live process was called the run's"
    );
    let control = HardKilledRun::new(&fixture.layout, "agent-2", "run-verify");
    assert!(run::recorded_run_is_live(&control.run_dir));

    assert!(
        engine::stale_recovery_pending(&fixture.layout, &fixture.data_root()),
        "the live control run was not seen"
    );
    let claim = fixture
        .claim()
        .expect("the verified control run is claimed");
    assert_names_agent_and_run(&claim, "agent-2", "run-verify");

    let reason = run::recover_ending(fixture.run_dir(), Duration::from_millis(50))
        .expect_err("a live, unverifiable group is reported, not reaped");
    assert!(
        reason.contains("unverified") || reason.contains("not signalled"),
        "{reason}"
    );
    assert!(
        fixture.live.alive(),
        "the unverifiable native child was signalled"
    );
}

#[test]
fn a_live_shell_identity_is_found_after_the_supervisor_record_dies() {
    let mut fixture = Fixture::hard_killed("live-shell");
    // The supervisor wrote the shell's positive identity at spawn. There is no
    // native child record in this variant; the shell is the durable handle.
    fs::remove_file(fixture.run_dir().join(run::NATIVE_IDENTITY_FILE)).expect("native.identity");
    fs::write(
        fixture.run_dir().join("shell.identity"),
        format!(
            "{} {} {} {}\n",
            fixture.live.native_pid,
            fixture.live.native_pgid,
            fixture.live.boot,
            fixture.live.native_start
        ),
    )
    .expect("shell.identity");
    fs::write(
        fixture.run_dir().join("shell.pid"),
        fixture.live.native_pid.to_string(),
    )
    .expect("shell.pid");
    fs::write(
        fixture.run_dir().join("shell.pgid"),
        fixture.live.native_pgid.to_string(),
    )
    .expect("shell.pgid");

    assert!(
        run::recorded_run_is_live(fixture.run_dir()),
        "the live shell identity was hidden by the dead supervisor record"
    );
    let claim = fixture.claim().expect("the live shell run is claimed");
    assert_names_agent_and_run(&claim, "agent-1", "run-1");
    let stopped = run::recover_ending(fixture.run_dir(), Duration::from_millis(250))
        .expect("recovery stops the verified shell group");
    assert!(stopped, "recovery found nothing to stop");
    fixture.live.wait_gone();
}
