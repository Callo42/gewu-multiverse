//! The agent process: one run, one child process, no model.
//!
//! This is what a run actually does. It is started by the terminal UI as an ordinary
//! child process of the tool, with the agent's workspace as its working directory. It
//! never runs as root, never installs a service and never uses a container runtime:
//! it is the same user, on the same machine, in a directory.
//!
//! A run, in sequence:
//!
//! 1. reads its plan and opens its transcript;
//! 2. installs the agent's skill set into the run directory, byte for byte;
//! 3. reads the agent's instruction document;
//! 4. executes the declared steps — the one declared tool this round is `shell`,
//!    run inside the workspace — recording each call and its result;
//! 5. records its own exit.
//!
//! **The workspace is a layout, not a security boundary.** The shell step is started
//! with the workspace as its working directory, and nothing else. It can still read
//! anything the user can read, and this code does not pretend otherwise: the tool
//! would need the operating system's help to confine a process, and it deliberately
//! asks for none. See `docs/agent-execution.md`.
//!
//! There is no model call anywhere in this module. The steps come from the agent's
//! own `steps.txt`, which is what stands in for a model's choices until the model
//! round. Remove the file and a run declares no steps.

use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::naming::agent_name_problem;
use crate::run::{RunOutcome, RunPlan};
use crate::transcript::{self, Record, SkillRef, Transcript};

/// The largest instruction document a run will read.
pub const MAX_INSTRUCTION_BYTES: u64 = 1024 * 1024;

/// How a run of the agent process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    /// The process's own exit code.
    pub exit_code: i32,
    /// What ended it, when there is something to say. Also printed to the log.
    pub reason: Option<String>,
}

impl RunEnd {
    /// A run that did everything it was asked to do.
    pub fn succeeded() -> Self {
        Self {
            exit_code: 0,
            reason: None,
        }
    }

    /// A run that stopped at a problem.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            reason: Some(reason.into()),
        }
    }

    /// Whether this run did everything it was asked to do.
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

/// The log of one run. The agent process owns it: it writes the tool's own lines
/// here and points every declared step's output at the same file, so one file holds
/// the whole story of the run. A copy of each line also goes to standard error, so a
/// person who started the run by hand can watch it; the terminal UI discards that
/// copy, because the file already has it.
struct RunLog {
    file: fs::File,
}

impl RunLog {
    /// Open `run_dir/run.log` for appending, creating it if it is absent.
    fn open(run_dir: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(run_dir.join(crate::run::LOG_FILE))?;
        Ok(Self { file })
    }

    /// One line, stamped and written to the file, with a copy on standard error.
    fn line(&mut self, text: &str) {
        let stamped = format!(
            "{} gewu-multiverse: {text}\n",
            crate::time::rfc3339(crate::time::now_unix_ms())
        );
        let _ = self.file.write_all(stamped.as_bytes());
        let _ = self.file.flush();
        eprintln!("gewu-multiverse: {text}");
    }

    /// A handle on the same file, for a declared step's own output.
    fn handle(&self) -> io::Result<fs::File> {
        self.file.try_clone()
    }
}

/// Run one agent run whose plan is already written in `run_dir`.
///
/// This never panics. Every problem becomes a sentence in the log and in the
/// transcript, and a non-zero exit code for the process that supervised the run.
pub fn execute(run_dir: &Path) -> RunEnd {
    let mut log = match RunLog::open(run_dir) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("gewu-multiverse: the run log cannot be opened: {error}");
            return RunEnd::failed(format!("the run log cannot be opened: {error}"));
        }
    };
    let plan = match RunPlan::read(run_dir) {
        Ok(plan) => plan,
        Err(error) => {
            log.line(&format!("the run plan cannot be read: {error}"));
            return RunEnd::failed(format!("the run plan cannot be read: {error}"));
        }
    };

    let mut transcript = match Transcript::open(run_dir) {
        Ok(transcript) => transcript,
        Err(error) => {
            log.line(&format!("the transcript cannot be opened: {error}"));
            return RunEnd::failed(format!("the transcript cannot be opened: {error}"));
        }
    };

    let started = Instant::now();
    let end = run(&plan, run_dir, &mut transcript, &mut log);
    if let Some(reason) = &end.reason {
        log.line(reason);
    }
    // The process's own exit is recorded before it exits, so a transcript read back
    // knows the run got this far even when the run failed.
    let _ = transcript.append(&Record::child_exit(Some(end.exit_code), None));
    let _ = transcript.append(&Record::run_finished(
        &plan.run_id,
        if end.is_success() {
            RunOutcome::Succeeded
        } else {
            RunOutcome::Failed
        },
        Some(end.exit_code),
        started.elapsed().as_millis() as u64,
        end.reason.clone(),
    ));
    log.line(&format!(
        "agent process exiting {}",
        match &end.reason {
            Some(reason) => format!("with code {} ({reason})", end.exit_code),
            None => "cleanly (exit 0)".to_owned(),
        }
    ));
    end
}

/// The body of a run, with the transcript and the log already open.
fn run(plan: &RunPlan, run_dir: &Path, transcript: &mut Transcript, log: &mut RunLog) -> RunEnd {
    let pid = std::process::id();
    log.line(&format!(
        "run {} of agent `{}` starting (pid {pid})",
        plan.run_id, plan.agent
    ));
    log.line(&format!("workspace: {}", plan.workspace.display()));
    if let Err(error) = transcript.append(&Record::run_started(
        &plan.run_id,
        &plan.agent,
        pid,
        &plan.workspace.display().to_string(),
    )) {
        return RunEnd::failed(format!("the transcript cannot be written: {error}"));
    }

    if let Err(reason) = fs::create_dir_all(&plan.workspace) {
        return RunEnd::failed(format!(
            "the workspace {} cannot be created: {reason}",
            plan.workspace.display()
        ));
    }

    let skills = match install_skills(&plan.skills_dir, run_dir) {
        Ok(skills) => skills,
        Err(reason) => return RunEnd::failed(reason),
    };
    log.line(&format!(
        "skills installed: {}",
        if skills.is_empty() {
            "none".to_owned()
        } else {
            skills
                .iter()
                .map(|skill| skill.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    if let Err(error) = transcript.append(&Record::skills_discovered(skills)) {
        return RunEnd::failed(format!("the transcript cannot be written: {error}"));
    }

    let instruction = match read_instruction(&plan.instruction) {
        Ok(instruction) => instruction,
        Err(reason) => return RunEnd::failed(reason),
    };
    log.line(&format!(
        "instruction read: {} ({} bytes)",
        plan.instruction.display(),
        instruction.len()
    ));
    if let Err(error) =
        transcript.append(&Record::instruction_read(&plan.instruction, &instruction))
    {
        return RunEnd::failed(format!("the transcript cannot be written: {error}"));
    }

    if plan.steps.is_empty() {
        log.line("no steps are declared, so the run has nothing to execute");
    }

    for (at, command) in plan.steps.iter().enumerate() {
        let index = at as u32 + 1;
        log.line(&format!("[step {index}] {command}"));
        if let Err(error) = transcript.append(&Record::tool_call(
            index,
            command,
            &plan.workspace.display().to_string(),
        )) {
            return RunEnd::failed(format!("the transcript cannot be written: {error}"));
        }
        let started = Instant::now();
        let step = match run_step(command, &plan.workspace, plan, log) {
            Ok(step) => step,
            Err(reason) => {
                let _ = transcript.append(&Record::tool_result(
                    index,
                    None,
                    None,
                    started.elapsed().as_millis() as u64,
                ));
                return RunEnd::failed(reason);
            }
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        log.line(&format!(
            "[step {index}] {} after {duration_ms} ms",
            match (step.exit_code, step.signal) {
                (Some(code), _) => format!("exit {code}"),
                (None, Some(signal)) => format!("ended by signal {signal}"),
                (None, None) => "ended with no status".to_owned(),
            }
        ));
        if let Err(error) = transcript.append(&Record::tool_result(
            index,
            step.exit_code,
            step.signal,
            duration_ms,
        )) {
            return RunEnd::failed(format!("the transcript cannot be written: {error}"));
        }
        if step.signal.is_some() || step.exit_code != Some(0) {
            return RunEnd {
                exit_code: step
                    .exit_code
                    .filter(|code| (1..=125).contains(code))
                    .unwrap_or(1),
                reason: Some(format!(
                    "declared step {index} {}",
                    match (step.exit_code, step.signal) {
                        (Some(code), _) => format!("exited with code {code}"),
                        (None, Some(signal)) => format!("was ended by signal {signal}"),
                        (None, None) => "ended with no status".to_owned(),
                    }
                )),
            };
        }
    }

    RunEnd::succeeded()
}

/// How one declared step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepEnd {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

/// Run one declared shell step inside the workspace.
///
/// The shell is started with the workspace as its working directory, and its output
/// joins the run's log. That is the whole intent: a step is meant to work in the
/// workspace. It is not confinement, and the documentation says so.
fn run_step(
    command: &str,
    workspace: &Path,
    plan: &RunPlan,
    log: &RunLog,
) -> Result<StepEnd, String> {
    let out = log
        .handle()
        .map_err(|error| format!("the run log cannot be opened again: {error}"))?;
    let err = log
        .handle()
        .map_err(|error| format!("the run log cannot be opened again: {error}"))?;
    let mut shell = Command::new("/bin/sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .env("GEWU_AGENT", &plan.agent)
        .env("GEWU_RUN_ID", &plan.run_id)
        .env("GEWU_WORKSPACE", &plan.workspace);
    let mut child = shell
        .spawn()
        .map_err(|error| format!("the shell step `{command}` cannot be started: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("the shell step `{command}` cannot be waited for: {error}"))?;
    Ok(StepEnd {
        exit_code: status.code(),
        signal: signal_of(status),
    })
}

/// The signal that ended a process, when one did.
#[cfg(unix)]
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Read the agent's instruction document.
fn read_instruction(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "the agent has no instruction document at {}; create it and press r, then start again",
                path.display()
            ));
        }
        Err(source) => {
            return Err(format!(
                "the instruction document {} cannot be described: {source}",
                path.display()
            ));
        }
    };
    if !metadata.is_file() {
        return Err(format!(
            "the instruction document {} is not a file",
            path.display()
        ));
    }
    if metadata.len() > MAX_INSTRUCTION_BYTES {
        return Err(format!(
            "the instruction document {} holds {} bytes, more than the {MAX_INSTRUCTION_BYTES} allowed",
            path.display(),
            metadata.len()
        ));
    }
    let bytes = fs::read(path).map_err(|source| {
        format!(
            "the instruction document {} cannot be read: {source}",
            path.display()
        )
    })?;
    if bytes.is_empty() {
        return Err(format!(
            "the instruction document {} is empty; write what the agent should do",
            path.display()
        ));
    }
    if bytes.contains(&0) {
        return Err(format!(
            "the instruction document {} holds a null byte; it must be plain text",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Install the effective skill set into the run directory, byte for byte.
///
/// A skill is one directory holding a `SKILL.md` and, optionally, more plain files.
/// An unsafe skill name, a skill without a manifest, a link or anything that is not
/// a plain file refuses the whole run: nothing is half-installed.
fn install_skills(skills_dir: &Path, run_dir: &Path) -> Result<Vec<SkillRef>, String> {
    let entries = match fs::read_dir(skills_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(format!(
                "the skills directory {} cannot be read: {source}",
                skills_dir.display()
            ));
        }
    };

    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|source| format!("the skills directory cannot be read: {source}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if !entry.path().is_dir() {
            return Err(format!(
                "the skill `{name}` is not a directory; a skill is a directory holding a SKILL.md"
            ));
        }
        if let Some(problem) = agent_name_problem(&name) {
            return Err(format!("the skill name `{name}` {problem}"));
        }
        names.push(name);
    }
    names.sort();

    let destination_root = run_dir.join("skills");
    fs::create_dir_all(&destination_root).map_err(|source| {
        format!(
            "the run's skills directory {} cannot be created: {source}",
            destination_root.display()
        )
    })?;

    let mut skills = Vec::new();
    for name in names {
        let source = skills_dir.join(&name);
        let manifest = source.join("SKILL.md");
        if !manifest.is_file() {
            return Err(format!(
                "the skill `{name}` has no SKILL.md; every skill needs one"
            ));
        }
        let destination = destination_root.join(&name);
        let stats = copy_tree(&source, &destination)?;
        let manifest_bytes = fs::read(&manifest).map_err(|source| {
            format!(
                "the skill manifest {} cannot be read: {source}",
                manifest.display()
            )
        })?;
        skills.push(SkillRef {
            name,
            files: stats.files,
            bytes: stats.bytes,
            digest: transcript::digest(&manifest_bytes),
        });
    }
    Ok(skills)
}

/// What a copied skill holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TreeStats {
    files: u32,
    bytes: u64,
}

/// Copy one skill directory into the run, refusing anything that is not a plain file.
///
/// Nothing here is a sandbox either; it is a refusal to follow a link out of a
/// skill's own directory, so a skill cannot smuggle in a file from elsewhere.
fn copy_tree(source: &Path, destination: &Path) -> Result<TreeStats, String> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "the skill directory {} cannot be created: {error}",
            destination.display()
        )
    })?;
    let mut stats = TreeStats::default();
    let entries = fs::read_dir(source)
        .map_err(|error| format!("the skill {} cannot be read: {error}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("a skill entry cannot be read: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("{} cannot be described: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "the skill entry {} is a link; a skill holds plain files only",
                path.display()
            ));
        }
        if metadata.is_dir() {
            let nested = copy_tree(&path, &destination.join(&name))?;
            stats.files += nested.files;
            stats.bytes += nested.bytes;
        } else if metadata.is_file() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
            fs::write(destination.join(&name), &bytes).map_err(|error| {
                format!(
                    "{} cannot be written: {error}",
                    destination.join(&name).display()
                )
            })?;
            stats.files += 1;
            stats.bytes += bytes.len() as u64;
        } else {
            return Err(format!(
                "the skill entry {} is neither a file nor a directory",
                path.display()
            ));
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;
    use std::path::PathBuf;

    fn workspace_source(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        // A scratch agent: an instruction, a skill set, and a workspace.
        let root = scratch_root(tag);
        let config = root.join("agents").join("scribe");
        let skills = config.join("skills");
        let workspace = config.join("workspace");
        fs::create_dir_all(&skills).expect("skills directory");
        fs::create_dir_all(&workspace).expect("workspace directory");
        (root, config, skills)
    }

    fn plan_for(run_dir: &Path, config: &Path, skills: &Path, steps: Vec<String>) -> RunPlan {
        RunPlan {
            run_id: "run-1".to_owned(),
            agent: "scribe".to_owned(),
            workspace: config.join("workspace"),
            instruction: config.join("instruction.md"),
            skills_dir: skills.to_path_buf(),
            steps,
            timeout_secs: 30,
        }
        .tap_write(run_dir)
    }

    /// A tiny helper so a test can write the plan in one expression.
    trait TapWrite: Sized {
        fn tap_write(self, run_dir: &Path) -> Self;
    }

    impl TapWrite for RunPlan {
        fn tap_write(self, run_dir: &Path) -> Self {
            self.write(run_dir).expect("the plan is written");
            self
        }
    }

    #[test]
    fn a_run_with_no_steps_succeeds_and_records_what_it_read() {
        let (root, config, skills) = workspace_source("agent-run-empty");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "do nothing, carefully\n").expect("instruction");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(end.is_success(), "{end:?}");

        let file = transcript::read(&run_dir).expect("the transcript reads");
        let events: Vec<&str> = file.records.iter().map(Record::event).collect();
        assert_eq!(
            events,
            vec![
                "run_started",
                "skills_discovered",
                "instruction_read",
                "child_exit",
                "run_finished"
            ]
        );
        let finished = transcript::terminal_record(&run_dir).expect("a terminal record");
        assert!(
            matches!(finished, Record::RunFinished { ref outcome, .. } if outcome == "succeeded")
        );
    }

    #[test]
    fn a_step_runs_in_the_workspace_and_its_effect_is_there_afterwards() {
        let (root, config, skills) = workspace_source("agent-run-step");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "make a note\n").expect("instruction");
        plan_for(
            &run_dir,
            &config,
            &skills,
            vec!["printf 'hello from the step\\n' > note.txt".to_owned()],
        );

        let end = execute(&run_dir);
        assert!(end.is_success(), "{end:?}");
        let note = config.join("workspace").join("note.txt");
        assert_eq!(
            fs::read_to_string(&note).expect("the step wrote the file"),
            "hello from the step\n"
        );

        let file = transcript::read(&run_dir).expect("the transcript reads");
        let call = file
            .records
            .iter()
            .find(|record| matches!(record, Record::ToolCall { .. }))
            .expect("a tool call is recorded");
        assert!(matches!(call, Record::ToolCall { tool, .. } if tool.as_str() == "shell"));
        let result = file
            .records
            .iter()
            .find(|record| matches!(record, Record::ToolResult { .. }))
            .expect("a tool result is recorded");
        assert!(matches!(
            result,
            Record::ToolResult {
                exit_code: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn a_failing_step_ends_the_run_with_that_code() {
        let (root, config, skills) = workspace_source("agent-run-fail");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "fail\n").expect("instruction");
        plan_for(
            &run_dir,
            &config,
            &skills,
            vec!["exit 7".to_owned(), "touch never-ran.txt".to_owned()],
        );

        let end = execute(&run_dir);
        assert_eq!(end.exit_code, 7);
        assert!(end.reason.expect("a reason").contains("step 1"));
        assert!(
            !config.join("workspace").join("never-ran.txt").exists(),
            "a run stops at the first failing step"
        );
    }

    #[test]
    fn a_missing_instruction_fails_the_run_with_a_sentence() {
        let (root, config, skills) = workspace_source("agent-run-no-instruction");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(!end.is_success());
        assert!(
            end.reason
                .expect("a reason")
                .contains("no instruction document"),
            "the reason names the missing document"
        );
    }

    #[test]
    fn skills_are_installed_byte_for_byte_and_recorded() {
        let (root, config, skills) = workspace_source("agent-run-skills");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let alpha = skills.join("alpha");
        fs::create_dir_all(&alpha).expect("skill directory");
        fs::write(alpha.join("SKILL.md"), "# alpha\n").expect("manifest");
        fs::write(alpha.join("notes.txt"), "more\n").expect("extra file");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(end.is_success(), "{end:?}");
        assert_eq!(
            fs::read_to_string(run_dir.join("skills/alpha/SKILL.md")).expect("installed"),
            "# alpha\n"
        );
        assert_eq!(
            fs::read_to_string(run_dir.join("skills/alpha/notes.txt")).expect("installed"),
            "more\n"
        );
        let file = transcript::read(&run_dir).expect("the transcript reads");
        let skills_record = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::SkillsDiscovered { skills, .. } => Some(skills),
                _ => None,
            })
            .expect("a skills record");
        assert_eq!(skills_record.len(), 1);
        assert_eq!(skills_record[0].name, "alpha");
        assert_eq!(skills_record[0].files, 2);
        assert!(skills_record[0].digest.starts_with("fnv1a64:"));
    }

    #[test]
    fn a_skill_without_a_manifest_refuses_the_whole_run() {
        let (root, config, skills) = workspace_source("agent-run-bad-skill");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        fs::create_dir_all(skills.join("empty")).expect("skill directory");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("no SKILL.md"));
        assert!(!run_dir.join("skills").join("empty").exists());
    }

    #[test]
    fn an_unsafe_skill_name_refuses_the_run() {
        let (root, config, skills) = workspace_source("agent-run-unsafe-skill");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let bad = skills.join("Bad Name");
        fs::create_dir_all(&bad).expect("skill directory");
        fs::write(bad.join("SKILL.md"), "# bad\n").expect("manifest");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("skill name"));
    }

    #[test]
    fn a_link_inside_a_skill_refuses_the_run() {
        let (root, config, skills) = workspace_source("agent-run-link-skill");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let alpha = skills.join("alpha");
        fs::create_dir_all(&alpha).expect("skill directory");
        fs::write(alpha.join("SKILL.md"), "# alpha\n").expect("manifest");
        std::os::unix::fs::symlink("/etc/passwd", alpha.join("outside"))
            .expect("a link is made for the test");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("link"));
    }

    #[test]
    fn an_empty_instruction_refuses_the_run() {
        let (root, config, skills) = workspace_source("agent-run-empty-instruction");
        let run_dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&run_dir).expect("run directory");
        fs::write(config.join("instruction.md"), "").expect("instruction");
        plan_for(&run_dir, &config, &skills, Vec::new());

        let end = execute(&run_dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("empty"));
    }
}
