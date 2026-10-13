//! Runs, driven through the real supervisor and the real binary.
//!
//! These tests start the program's own executable in its agent mode, exactly as the
//! terminal UI does, and then read what the run left behind: the plan, the log and
//! the append-only JSONL transcript. No model is involved and no network is touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gewu_multiverse::paths::{AgentPaths, Layout};
use gewu_multiverse::run::{RunOutcome, RunPlan};
use gewu_multiverse::supervisor::{StartDisposition, State, StopDisposition, Supervisor};
use gewu_multiverse::transcript::{self, Record};

/// The real program, which is also the agent process.
const BINARY: &str = env!("CARGO_BIN_EXE_gewu-multiverse");

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-runs-{tag}-{}-{nanos}", process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// An agent with an instruction and a skill set, ready for a run.
fn agent(root: &Path, name: &str, instruction: &str) -> AgentPaths {
    let layout = Layout::rooted_at(root);
    let paths = layout.ensure_agent(name).expect("the agent is prepared");
    fs::write(paths.instruction(), instruction).expect("the instruction is written");
    let skill = paths.skills_dir().join("alpha");
    fs::create_dir_all(&skill).expect("the skill directory is created");
    fs::write(skill.join("SKILL.md"), "# alpha\n").expect("the skill manifest is written");
    fs::write(skill.join("notes.txt"), "more\n").expect("a skill file is written");
    paths
}

fn supervisor(root: &Path, timeout: Duration, max_concurrent: usize) -> Supervisor {
    Supervisor::new(
        Layout::rooted_at(root).state_root(),
        BINARY,
        max_concurrent,
        timeout,
    )
}

/// Poll until the agent reaches a terminal state, or fail after a while.
fn wait_for_finish(supervisor: &mut Supervisor, name: &str) -> State {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        supervisor.poll(Instant::now());
        let state = supervisor.state(name);
        if matches!(state, State::Finished { .. }) {
            return state;
        }
        if Instant::now() >= deadline {
            panic!("the run of {name} did not finish; last state {state:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn process_is_gone(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn a_real_run_leaves_a_plan_a_log_a_transcript_and_its_effects() {
    let root = scratch("real-run");
    let paths = agent(&root, "scribe", "make a note, then finish\n");
    fs::write(
        paths.steps_file(),
        "# a comment line\nprintf 'hello from a declared step\\n' > note.txt\n",
    )
    .expect("the steps file is written");
    let run_id = "run-1";
    let run_dir = paths.run_dir(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    RunPlan {
        run_id: run_id.to_owned(),
        agent: "scribe".to_owned(),
        workspace: paths.workspace().to_path_buf(),
        instruction: paths.instruction().to_path_buf(),
        skills_dir: paths.skills_dir().to_path_buf(),
        steps: RunPlan::read_steps(paths.steps_file()).expect("the steps can be read"),
        timeout_secs: 30,
    }
    .write(&run_dir)
    .expect("the plan is written");

    let status = process::Command::new(BINARY)
        .arg("--run-agent")
        .arg(&run_dir)
        .status()
        .expect("the agent process runs");
    assert!(status.success(), "the run exited {status:?}");

    // The workspace holds what the declared step did.
    assert_eq!(
        fs::read_to_string(paths.workspace().join("note.txt")).expect("the step wrote it"),
        "hello from a declared step\n"
    );
    // The log holds the agent process's own output.
    let log = fs::read_to_string(run_dir.join("run.log")).expect("the log is readable");
    assert!(
        log.contains("run run-1 of agent `scribe` starting"),
        "{log}"
    );
    assert!(log.contains("[step 1]"), "{log}");
    assert!(log.contains("hello from a declared step"), "{log}");
    // The transcript is one JSON object per line, with the run's story in it.
    let file = transcript::read(&run_dir).expect("the transcript reads back");
    let events: Vec<&str> = file.records.iter().map(Record::event).collect();
    assert_eq!(
        events,
        vec![
            "run_started",
            "skills_discovered",
            "instruction_read",
            "tool_call",
            "tool_result",
            "child_exit",
            "run_finished"
        ],
        "{events:?}"
    );
    let skill = file
        .records
        .iter()
        .find_map(|record| match record {
            Record::SkillsDiscovered { skills, .. } => skills.first(),
            _ => None,
        })
        .expect("the skill record");
    assert_eq!(skill.name, "alpha");
    assert_eq!(skill.files, 2);
    assert_eq!(
        fs::read_to_string(run_dir.join("skills/alpha/SKILL.md")).expect("installed"),
        "# alpha\n"
    );
    assert!(matches!(
        transcript::terminal_record(&run_dir),
        Some(Record::RunFinished { ref outcome, .. }) if outcome == "succeeded"
    ));
}

#[test]
fn the_supervisor_starts_a_run_and_records_its_outcome() {
    let root = scratch("supervised-run");
    let paths = agent(&root, "scribe", "do nothing\n");
    fs::write(paths.steps_file(), "true\n").expect("the steps file is written");
    let mut supervisor = supervisor(&root, Duration::from_secs(30), 10);
    let steps = RunPlan::read_steps(paths.steps_file()).expect("steps");
    let disposition = supervisor.start(&paths, steps).expect("started");
    let run_id = match disposition {
        StartDisposition::Started { run_id, .. } => run_id,
        other => panic!("unexpected {other:?}"),
    };
    let state = wait_for_finish(&mut supervisor, "scribe");
    match state {
        State::Finished { outcome, .. } => assert_eq!(outcome, RunOutcome::Succeeded),
        other => panic!("unexpected {other:?}"),
    }
    let run_dir = supervisor.run_dir("scribe", &run_id);
    assert!(run_dir.join("plan.json").is_file());
    assert!(run_dir.join("run.log").is_file());
    assert!(transcript::terminal_record(&run_dir).is_some());
}

#[test]
fn stopping_an_agent_stops_its_whole_process_tree() {
    let root = scratch("stop-tree");
    let paths = agent(&root, "scribe", "start something and wait\n");
    fs::write(paths.steps_file(), "sleep 300 & echo $! > tree.pid; wait\n")
        .expect("the steps file is written");
    let mut supervisor = supervisor(&root, Duration::from_secs(60), 10);
    let steps = RunPlan::read_steps(paths.steps_file()).expect("steps");
    let disposition = supervisor.start(&paths, steps).expect("started");
    let child_pid = match disposition {
        StartDisposition::Started { pid, .. } => pid,
        other => panic!("unexpected {other:?}"),
    };

    // Wait until the declared step has started its own background process.
    let tree_pid_path = paths.workspace().join("tree.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !tree_pid_path.is_file() {
        supervisor.poll(Instant::now());
        assert!(Instant::now() < deadline, "the step never started its tree");
        thread::sleep(Duration::from_millis(20));
    }
    let tree_pid: u32 = fs::read_to_string(&tree_pid_path)
        .expect("the tree pid is written")
        .trim()
        .parse()
        .expect("the tree pid is a number");
    assert!(
        Path::new(&format!("/proc/{tree_pid}")).exists(),
        "the tree is alive"
    );

    assert_eq!(supervisor.stop("scribe"), StopDisposition::Signalled);
    let state = wait_for_finish(&mut supervisor, "scribe");
    match state {
        State::Finished { outcome, .. } => assert_eq!(outcome, RunOutcome::Stopped),
        other => panic!("unexpected {other:?}"),
    }
    // The proof: both the agent process and the process it started are gone.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(process_is_gone(child_pid) && process_is_gone(tree_pid)) {
        assert!(
            Instant::now() < deadline,
            "the process tree is still there: agent {child_pid} gone={}, started {tree_pid} gone={}",
            process_is_gone(child_pid),
            process_is_gone(tree_pid)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_overrunning_run_is_stopped_by_its_timeout_and_reported() {
    let root = scratch("timeout");
    let paths = agent(&root, "scribe", "overrun\n");
    fs::write(paths.steps_file(), "sleep 300\n").expect("the steps file is written");
    let mut supervisor = supervisor(&root, Duration::from_millis(500), 10);
    let steps = RunPlan::read_steps(paths.steps_file()).expect("steps");
    let disposition = supervisor.start(&paths, steps).expect("started");
    let (run_id, child_pid) = match disposition {
        StartDisposition::Started { run_id, pid } => (run_id, pid),
        other => panic!("unexpected {other:?}"),
    };
    let state = wait_for_finish(&mut supervisor, "scribe");
    match state {
        State::Finished {
            outcome, reason, ..
        } => {
            assert_eq!(outcome, RunOutcome::TimedOut);
            let reason = reason.expect("a reason");
            assert!(reason.contains("timeout"), "{reason}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(process_is_gone(child_pid), "the agent process is gone");
    let run_dir = supervisor.run_dir("scribe", &run_id);
    let terminal = transcript::terminal_record(&run_dir).expect("a terminal record");
    assert!(matches!(
        terminal,
        Record::RunFinished { ref outcome, ref reason, .. }
            if outcome == "timed_out" && reason.as_deref().unwrap_or("").contains("timeout")
    ));
    let log = fs::read_to_string(run_dir.join("run.log")).expect("the log is readable");
    assert!(log.contains("reached its timeout"), "{log}");
}

#[test]
fn a_start_beyond_the_limit_waits_its_turn() {
    let root = scratch("limit-wait");
    let layout = Layout::rooted_at(&root);
    let one = agent(&root, "one", "wait\n");
    let two = layout.ensure_agent("two").expect("two is prepared");
    fs::write(two.instruction(), "wait too\n").expect("instruction");
    for paths in [&one, &two] {
        fs::write(paths.steps_file(), "sleep 300\n").expect("the steps file is written");
    }
    let mut supervisor = supervisor(&root, Duration::from_secs(60), 1);
    let first = supervisor
        .start(&one, RunPlan::read_steps(one.steps_file()).expect("steps"))
        .expect("started");
    assert!(matches!(first, StartDisposition::Started { .. }));
    let second = supervisor
        .start(&two, RunPlan::read_steps(two.steps_file()).expect("steps"))
        .expect("queued");
    match second {
        StartDisposition::Queued { position, .. } => assert_eq!(position, 1),
        other => panic!("expected a wait, got {other:?}"),
    }
    assert_eq!(supervisor.waiting_count(), 1);

    // Free the slot and watch the start that was waiting begin.
    supervisor.stop("one");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        supervisor.poll(Instant::now());
        let state = supervisor.state("two");
        if matches!(&state, State::Starting { .. } | State::Running { .. }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the waiting start never began; state {state:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    supervisor.stop_all(Duration::from_secs(10));
}
