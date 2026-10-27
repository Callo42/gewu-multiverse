//! A native helper may call setsid and leave the leader's process group while
//! still holding the run's anonymous capability. Settlement must close it before
//! the run's identity records are removed.
#[path = "../src/test_wait.rs"]
mod test_wait;
use std::process::{Command, Stdio};

/// Both probes scan the process table for an anonymous capability holder. Run
/// them one at a time so one probe's helper cannot be attributed to the other.
static CAPABILITY_PROBE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gewu-helper-settle-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("probe run dir");
    dir
}

/// The kernel session a pid belongs to, from field 6 of `/proc/<pid>/stat`.
fn session_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

#[test]
fn a_setsid_helper_holding_the_run_capability_is_found_and_closed() {
    let _serial = CAPABILITY_PROBE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let dir = run_dir("scan");
    let _capability = gewu_multiverse::run::prepare_shell_capability(&dir)
        .expect("the probe capability is recorded");
    let mut child = Command::new("/usr/bin/setsid")
        .arg("/bin/sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the setsid helper starts");
    test_wait::wait_until("the setsid helper's own session", || {
        session_of(child.id()) == Some(child.id())
    });
    let result = gewu_multiverse::run::recover_ending(&dir, test_wait::LIVENESS_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();
    let reaped = result.expect("the existing capability scan must close the helper");
    assert!(reaped, "the setsid helper was not found/reaped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn normal_settle_closes_a_setsid_helper_before_clearing_records() {
    let _serial = CAPABILITY_PROBE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let dir = run_dir("settle");
    let _capability = gewu_multiverse::run::prepare_shell_capability(&dir)
        .expect("the probe capability is recorded");
    let leader_file = dir.join("leader.pid");
    let helper_file = dir.join("helper.pid");
    let script = format!(
        "echo $$ > {}; setsid /bin/sleep 30 & echo $! > {}; wait",
        leader_file.display(),
        helper_file.display()
    );
    let mut leader = Command::new("/usr/bin/setsid")
        .arg("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the leader starts");
    test_wait::wait_until("the leader and helper pid records", || {
        leader_file.exists() && helper_file.exists()
    });
    let leader_pid: u32 = std::fs::read_to_string(&leader_file)
        .expect("leader pid")
        .trim()
        .parse()
        .expect("leader pid number");
    let helper_pid: u32 = std::fs::read_to_string(&helper_file)
        .expect("helper pid")
        .trim()
        .parse()
        .expect("helper pid number");
    let settled = gewu_multiverse::run::request_finish(
        &dir,
        leader_pid,
        gewu_multiverse::run::RunOutcome::Stopped,
        None,
        test_wait::LIVENESS_TIMEOUT,
        Some("the run was stopped on request".to_owned()),
        gewu_multiverse::run::FirstEnding::Stop,
    );
    // A zombie has ended; only its reaping is left, and this test is not its
    // parent. The product's own group scan ignores zombies for the same reason.
    let helper_alive = std::fs::read_to_string(format!("/proc/{helper_pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next().map(|state| state != "Z"))
        })
        .unwrap_or(false);
    if helper_alive {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{helper_pid}"))
            .status();
    }
    let _ = leader.kill();
    let _ = leader.wait();
    let _ = std::fs::remove_dir_all(&dir);
    settled.expect("the leader group settles");
    assert!(!helper_alive, "the setsid helper survived normal settle");
}
