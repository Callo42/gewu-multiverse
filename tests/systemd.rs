//! The real thing: a user unit written, started, enabled, disabled and stopped with
//! the machine's own `systemctl --user`, and a run that fails until its declared
//! restart bound stops it.
//!
//! These tests are written to be honest on a machine that cannot do this. A machine
//! with no user session (a bare container, a serial console with no login manager)
//! has no user systemd to talk to; the test says so, in one line, and passes without
//! pretending it proved anything. The unit-level tests in `src/systemd.rs` and the
//! stand-in tests in `src/supervisor.rs` cover the rendering, the commands and the
//! state mapping on every machine.
//!
//! Nothing here is left behind: every unit is stopped, disabled and removed on every
//! way out, including a panic, by a guard that owns it.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::systemd::{
    RestartPolicy, Systemctl, UnitRun, UnitSpec, UnitStatus, parse_show, render_unit, state_of,
    unit_name, user_unit_dir, write_unit,
};

/// One unit a test wrote, removed however the test ends.
struct Probe {
    client: Systemctl,
    dir: Option<PathBuf>,
    agent: String,
    name: String,
    log: PathBuf,
}

impl Probe {
    /// Take a unique unit name, and the real unit directory of this user.
    fn new(tag: &str) -> Option<Self> {
        let client = Systemctl::new();
        if let Err(reason) = client.available() {
            eprintln!("SKIPPED ({tag}): there is no user systemd to talk to: {reason}");
            return None;
        }
        let dir = user_unit_dir()?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let agent = format!("probe-{}-{}", std::process::id(), nanos % 1_000_000);
        let name = unit_name(&agent);
        let log = std::env::temp_dir().join(format!("{name}.log"));
        Some(Self {
            client,
            dir: Some(dir),
            agent,
            name,
            log,
        })
    }

    fn dir(&self) -> &PathBuf {
        self.dir.as_ref().expect("the unit directory is known")
    }

    /// Write one unit into the user's own systemd directory.
    fn write(&self, spec: &UnitSpec) -> PathBuf {
        let path = write_unit(self.dir(), spec).expect("the unit is written");
        self.client
            .daemon_reload()
            .expect("systemd reads its units again");
        path
    }

    fn status(&self) -> gewu_multiverse::systemd::UnitStatus {
        self.client
            .show(&self.name)
            .expect("systemd reports the unit")
    }

    /// Wait until the unit is in `want`, or give up.
    fn wait_for(&self, want: &str, budget: Duration) -> gewu_multiverse::systemd::UnitStatus {
        let deadline = Instant::now() + budget;
        loop {
            let status = self.status();
            if status.active_state == want || Instant::now() >= deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Every way out: stop, disable, remove the unit file, and ask systemd to read
        // its units again. The unit is this test's own, so nothing else is touched.
        let _ = self.client.stop(&self.name);
        let _ = self.client.disable(&self.name);
        if let Some(dir) = &self.dir {
            let _ = fs::remove_file(dir.join(&self.name));
            let _ = fs::remove_file(dir.join("default.target.wants").join(&self.name));
            let _ = fs::remove_dir(dir.join("default.target.wants"));
            let _ = fs::remove_dir(dir);
            // The directory above it was made for this unit directory; remove it too
            // when it is empty, so the machine is left as it was found.
            if let Some(parent) = dir.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
        // A unit this test wrote must not stay reported as failed by systemd.
        let _ = self.client.reset_failed(&self.name);
        let _ = fs::remove_file(&self.log);
        let _ = self.client.daemon_reload();
    }
}

#[test]
fn a_rendered_unit_and_the_status_mapping_can_be_read_back_for_review() {
    // `cargo test --test systemd -- --nocapture` prints this, which is how the unit
    // and the state mapping in a round's report were captured. It proves nothing on
    // its own beyond determinism and that the mapping is a pure function of what
    // systemd says; the assertions that matter are in the tests above and beside it.
    let spec = UnitSpec::for_run(
        "scribe",
        PathBuf::from("/home/you/.cargo/bin/gewu-multiverse").as_path(),
        PathBuf::from("/home/you/.local/state/gewu-multiverse/scribe/run-1760000000000-0001")
            .as_path(),
        PathBuf::from("/home/you/.config/gewu-multiverse/agents/scribe/workspace").as_path(),
        PathBuf::from(
            "/home/you/.local/state/gewu-multiverse/scribe/run-1760000000000-0001/run.log",
        )
        .as_path(),
        RestartPolicy::default(),
    );
    let unit = render_unit(&spec);
    assert_eq!(unit, render_unit(&spec), "the rendering is deterministic");
    println!("===== gewu-agent-scribe.service =====\n{unit}");

    let run_dir = std::env::temp_dir().join(format!("gewu-mapping-{}", std::process::id()));
    std::fs::create_dir_all(&run_dir).expect("a run directory for the mapping");
    {
        let mut transcript =
            gewu_multiverse::transcript::Transcript::open(&run_dir).expect("a transcript");
        transcript
            .append(&gewu_multiverse::transcript::Record::run_started(
                "run-1760000000000-0001",
                "scribe",
                1,
                "/w",
            ))
            .expect("a record");
        transcript
            .append(&gewu_multiverse::transcript::Record::run_finished(
                "run-1760000000000-0001",
                gewu_multiverse::run::RunOutcome::Succeeded,
                Some(0),
                40,
                None,
            ))
            .expect("a record");
    }
    let run = UnitRun {
        run_id: "run-1760000000000-0001".to_owned(),
        run_dir: run_dir.clone(),
    };
    println!("===== status mapping =====");
    for (text, with_run) in [
        ("LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=4321\nActiveEnterTimestampMonotonic=1\n", true),
        ("LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nMainPID=0\n", true),
        ("LoadState=loaded\nActiveState=deactivating\nSubState=stop-sigterm\nMainPID=4321\n", true),
        ("LoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n", true),
        // A unit that failed before any run wrote a record of its own.
        ("LoadState=loaded\nActiveState=failed\nResult=exit-code\nExecMainStatus=1\n", false),
        ("LoadState=not-found\nActiveState=inactive\nSubState=dead\n", false),
    ] {
        let status = UnitStatus::from_map(&parse_show(text));
        let selected = if with_run { Some(&run) } else { None };
        let state = state_of(&status, selected, 3_000_000);
        println!(
            "{:>13}/{:>14} -> {}",
            status.load_state,
            status.active_state,
            state.word()
        );
        assert!(!state.is_live() || status.is_live());
    }
    let _ = std::fs::remove_dir_all(&run_dir);
}

#[test]
fn a_real_user_unit_starts_enables_disables_and_stops() {
    let Some(probe) = Probe::new("start/stop") else {
        return;
    };
    let spec = UnitSpec {
        agent: probe.agent.clone(),
        exec: vec![PathBuf::from("/bin/sleep"), PathBuf::from("30")],
        workspace: std::env::temp_dir(),
        log: probe.log.clone(),
        restart: RestartPolicy::default(),
    };
    let unit_path = probe.write(&spec);

    probe.client.start(&probe.name).expect("the unit starts");
    let status = probe.wait_for("active", Duration::from_secs(10));
    assert_eq!(status.active_state, "active", "{status:?}");
    assert_eq!(status.sub_state, "running", "{status:?}");
    assert!(status.main_pid.is_some(), "systemd reports the main process");

    // `systemctl --user status` is the command a person would run; it must see it too.
    let seen = Command::new("systemctl")
        .args(["--user", "status", &probe.name])
        .output()
        .expect("systemctl runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&seen.stdout),
        String::from_utf8_lossy(&seen.stderr)
    );
    assert!(
        text.contains(&probe.name) || seen.status.success(),
        "{text}"
    );

    // Enabling and disabling is how the unit would be made to start at login; both
    // are asked for here and undone by the guard.
    probe
        .client
        .enable(&probe.name)
        .expect("the unit is enabled");
    probe
        .client
        .disable(&probe.name)
        .expect("the unit is disabled");

    probe.client.stop(&probe.name).expect("the unit stops");
    let status = probe.wait_for("inactive", Duration::from_secs(10));
    assert_eq!(status.active_state, "inactive", "{status:?}");
    assert!(!status.is_live(), "an inactive unit is not alive");

    assert!(unit_path.is_file(), "the unit file is still there");
    drop(probe);
    assert!(
        !unit_path.exists(),
        "the unit file is removed when the test ends"
    );
}

#[test]
fn a_real_restart_bound_stops_a_run_that_keeps_failing() {
    let Some(probe) = Probe::new("restart bound") else {
        return;
    };
    // A declared policy that is easy to see: restart a failure after one second, at
    // most two starts inside the interval, and then leave the unit failed.
    let policy = RestartPolicy {
        restart_secs: 1,
        burst: 2,
        interval_secs: 60,
    };
    let spec = UnitSpec {
        agent: probe.agent.clone(),
        exec: vec![PathBuf::from("/bin/false")],
        workspace: std::env::temp_dir(),
        log: probe.log.clone(),
        restart: policy,
    };
    let unit_path = probe.write(&spec);
    let unit = fs::read_to_string(&unit_path).expect("the unit reads");
    assert!(unit.contains("Restart=on-failure"), "{unit}");
    assert!(unit.contains("RestartSec=1"), "{unit}");
    assert!(unit.contains("StartLimitBurst=2"), "{unit}");

    probe.client.start(&probe.name).expect("the unit starts");
    let status = probe.wait_for("failed", Duration::from_secs(20));
    assert_eq!(status.active_state, "failed", "{status:?}");
    assert!(
        status.n_restarts >= 1,
        "systemd restarted the failed run: {status:?}"
    );
    eprintln!(
        "{} failed after {} restart(s) under Restart=on-failure, bounded by StartLimitBurst={} in {} s",
        probe.name, status.n_restarts, policy.burst, policy.interval_secs
    );
}
