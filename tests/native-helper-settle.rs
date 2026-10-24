//! A native helper may call setsid and leave the leader's process group while
//! still holding the run's anonymous capability. Settlement must close it before
//! the run's identity records are removed.
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

#[test]
fn a_setsid_helper_holding_the_run_capability_is_found_and_closed() {
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
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2)
        && Path::new(&format!("/proc/{}", child.id())).exists()
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = gewu_multiverse::run::reap_leftover_run(&dir, Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    let reaped = result.expect("the existing capability scan must close the helper");
    assert!(reaped, "the setsid helper was not found/reaped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn normal_settle_closes_a_setsid_helper_before_clearing_records() {
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
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2)
        && (!leader_file.exists() || !helper_file.exists())
    {
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let settled = gewu_multiverse::run::settle_run(
        &dir,
        leader_pid,
        gewu_multiverse::run::RunOutcome::Stopped,
        None,
        Duration::from_secs(2),
        gewu_multiverse::run::FirstEnding::Stop,
    );
    let helper_alive = Path::new(&format!("/proc/{helper_pid}")).exists();
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
