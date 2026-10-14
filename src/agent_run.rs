//! The agent process: one run, one child process, one model loop.
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
//! 4. reads the agent's model key out of the credentials file, and the other secrets
//!    beside it, before anything runs, so that any output can be redacted with them;
//! 5. runs the agent's declared steps, if it declares any, as a fixed prelude: they
//!    are the agent's own setup, and they run in the workspace like every tool;
//! 6. **calls the model**, one turn at a time: it sends the instruction, the
//!    conversation so far and the declared tool descriptions; it runs the tool calls
//!    the model asks for inside the workspace; it feeds the results back; and it
//!    repeats until the model answers without a tool call, or the declared bound of
//!    [`RunPlan::max_turns`] turns is reached;
//! 7. records its own exit and the reason the loop ended.
//!
//! **The workspace is a layout, not a security boundary.** A tool is started with the
//! workspace as its working directory, and nothing else. It can still read anything
//! the user can read, and this code does not pretend otherwise: the tool would need
//! the operating system's help to confine a process, and it deliberately asks for
//! none. See `docs/agent-execution.md`.
//!
//! Every model exchange lands in the transcript as it happened: the request as sent
//! with the key redacted, the reply as received, and every tool call with its result.
//! The key itself leaves this process in exactly one place — the `Authorization`
//! header of the request — and every string that is written down is passed through
//! [`Secret::redact`] first — with every other secret the credentials file holds — so a
//! run can be read afterwards without any secret being recoverable.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::credentials::{self, Secret};
use crate::model::{self, Message, ModelConfig, ToolCall};
use crate::naming::agent_name_problem;
use crate::run::{RunOutcome, RunPlan};
use crate::transcript::{self, ModelToolCall, ModelUsage, Record, SkillRef, Transcript};

/// The largest instruction document a run will read.
pub const MAX_INSTRUCTION_BYTES: u64 = 1024 * 1024;

/// The largest amount of a single tool's output that is kept, over both streams.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

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

    /// A run that finished with something to say, such as "the model finished".
    pub fn completed(reason: impl Into<String>) -> Self {
        Self {
            exit_code: 0,
            reason: Some(reason.into()),
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
/// here and points every tool's output at the same file, so one file holds the whole
/// story of the run. A copy of each line also goes to standard error, so a person who
/// started the run by hand can watch it; the terminal UI discards that copy, because
/// the file already has it.
///
/// The log holds the run's [`Secret`], and every line and every block of output is
/// passed through it before being written, so the key cannot reach the file.
struct RunLog {
    file: fs::File,
    secret: Secret,
}

impl RunLog {
    /// Open `run_dir/run.log` for appending, creating it if it is absent.
    fn open(run_dir: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(crate::mode::SECRET_FILE_MODE)
            .open(run_dir.join(crate::run::LOG_FILE))?;
        Ok(Self {
            file,
            secret: Secret::empty(),
        })
    }

    /// Start redacting with this run's key.
    fn set_secret(&mut self, secret: Secret) {
        self.secret = secret;
    }

    /// The run's key, for redacting anything else that is written down.
    fn secret(&self) -> &Secret {
        &self.secret
    }

    /// One line, stamped, redacted, written to the file, with a copy on standard error.
    fn line(&mut self, text: &str) {
        let redacted = self.secret.redact(text);
        let stamped = format!(
            "{} gewu-multiverse: {redacted}\n",
            crate::time::rfc3339(crate::time::now_unix_ms())
        );
        let _ = self.file.write_all(stamped.as_bytes());
        let _ = self.file.flush();
        eprintln!("gewu-multiverse: {redacted}");
    }

    /// A block of a tool's own output, redacted, written as it came.
    fn block(&mut self, text: &str) {
        let redacted = self.secret.redact(text);
        let _ = self.file.write_all(redacted.as_bytes());
        let _ = self.file.flush();
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
    // The reason was just written as its own line; do not repeat it here, so a
    // failure sentence appears in the log exactly once.
    match &end.reason {
        Some(_) => log.line(&format!("agent process exiting with code {}", end.exit_code)),
        None => log.line("agent process exiting cleanly (exit 0)"),
    }
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

    // The key is read here, before anything runs, so that a declared step's output is
    // redacted too. A run without a key cannot proceed: it would have nothing to think
    // with, and pretending otherwise would only hide the problem.
    let secret = match credentials::model_key(&plan.credentials, &plan.agent) {
        Ok(secret) => secret,
        Err(error) => {
            let reason = format!("the model key cannot be read: {error}");
            // The sentence is written to the log once, by `execute`, from the run's
            // own reason; writing it here as well made it appear twice.
            let _ = transcript.append(&Record::model_call_failed(
                &plan.run_id,
                0,
                "missing_key",
                &reason,
            ));
            return RunEnd::failed(reason);
        }
    };
    // Redact every secret the credentials file holds, not only this run's model key: a
    // tool can print that file, and whatever it prints reaches the log and the
    // transcript. The model key stays what the request's Authorization header uses.
    let secret = secret.with_extra_redactions(credentials::all_secret_values(&plan.credentials));
    log.set_secret(secret.clone());
    log.line(&format!(
        "model: {} at {} (bound: {} turn(s), {} s)",
        plan.model, plan.endpoint, plan.max_turns, plan.timeout_secs
    ));
    // The bound is written into the transcript before the first call, so a run read
    // afterwards shows what it was held to rather than only how it ended.
    if let Err(error) = transcript.append(&Record::run_limits(
        &plan.run_id,
        plan.max_turns,
        plan.timeout_secs,
        &plan.endpoint,
        &plan.model,
    )) {
        return RunEnd::failed(format!("the transcript cannot be written: {error}"));
    }

    // The declared steps, as a fixed prelude. They are what the agent itself placed in
    // steps.txt; a run then hands the real decisions to the model.
    let mut tool_index: u32 = 0;
    for command in &plan.steps {
        tool_index += 1;
        if let Err(end) = run_declared_step(tool_index, command, plan, transcript, log) {
            return end;
        }
    }

    model_loop(plan, &instruction, &secret, tool_index, transcript, log)
}

/// Run one declared step, recording it exactly like a model-asked tool call.
fn run_declared_step(
    index: u32,
    command: &str,
    plan: &RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) -> Result<(), RunEnd> {
    log.line(&format!("[step {index}] {command}"));
    let workspace = plan.workspace.display().to_string();
    if let Err(error) = transcript.append(&Record::tool_call(
        None,
        index,
        model::SHELL_TOOL,
        command,
        &workspace,
    )) {
        return Err(RunEnd::failed(format!(
            "the transcript cannot be written: {error}"
        )));
    }
    let started = Instant::now();
    let outcome = match run_shell(command, plan, log) {
        Ok(outcome) => outcome,
        Err(reason) => {
            let _ = transcript.append(&Record::tool_result(
                None,
                index,
                model::SHELL_TOOL,
                None,
                None,
                started.elapsed().as_millis() as u64,
                None,
            ));
            return Err(RunEnd::failed(reason));
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let end = describe_end(outcome.exit_code, outcome.signal);
    log.line(&format!("[step {index}] {end} after {duration_ms} ms"));
    if let Err(error) = transcript.append(&Record::tool_result(
        None,
        index,
        model::SHELL_TOOL,
        outcome.exit_code,
        outcome.signal,
        duration_ms,
        Some(outcome.excerpt.clone()),
    )) {
        return Err(RunEnd::failed(format!(
            "the transcript cannot be written: {error}"
        )));
    }
    if outcome.signal.is_some() || outcome.exit_code != Some(0) {
        return Err(RunEnd {
            exit_code: outcome
                .exit_code
                .filter(|code| (1..=125).contains(code))
                .unwrap_or(1),
            reason: Some(format!("declared step {index} {end}")),
        });
    }
    Ok(())
}

/// The whole model loop: one turn at a time, until the model finishes or a bound ends it.
fn model_loop(
    plan: &RunPlan,
    instruction: &[u8],
    secret: &Secret,
    mut tool_index: u32,
    transcript: &mut Transcript,
    log: &mut RunLog,
) -> RunEnd {
    let instruction_text = String::from_utf8_lossy(instruction).into_owned();
    let mut messages = vec![
        Message::system(system_prompt(plan)),
        Message::user(instruction_text),
    ];
    let declared_tools = model::tools();
    let deadline = Instant::now() + Duration::from_secs(plan.timeout_secs);
    let mut turn: u32 = 0;

    loop {
        if turn >= plan.max_turns {
            return RunEnd::failed(format!(
                "the run reached its declared maximum of {} turn(s) (bound: max_turns) and the model still asked to continue",
                plan.max_turns
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return RunEnd::failed(format!(
                "the run reached its declared timeout of {} s (bound: run_timeout_secs)",
                plan.timeout_secs
            ));
        }
        turn += 1;

        let config = ModelConfig {
            endpoint: plan.endpoint.clone(),
            model: plan.model.clone(),
            api_key: secret.clone(),
            timeout: model::DEFAULT_TIMEOUT.min(remaining),
        };
        let exchange = match model::chat(
            &config,
            &messages,
            &declared_tools,
            Some(model::DEFAULT_MAX_TOKENS),
        ) {
            Ok(exchange) => exchange,
            Err(error) => {
                let class = error.class();
                let reason = format!("the model call failed ({class}): {error}");
                // Written once, by `execute`, from the run's own reason.
                let _ = transcript.append(&Record::model_call_failed(
                    &plan.run_id,
                    turn,
                    class,
                    &error.to_string(),
                ));
                return RunEnd::failed(reason);
            }
        };

        if let Err(error) = transcript.append(&Record::model_request(
            &plan.run_id,
            turn,
            &plan.endpoint,
            &plan.model,
            &exchange.authorization,
            &exchange.request_body,
        )) {
            return RunEnd::failed(format!("the transcript cannot be written: {error}"));
        }

        let reply = exchange.reply;
        let recorded_calls: Vec<ModelToolCall> = reply
            .tool_calls
            .iter()
            .map(|call| ModelToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            })
            .collect();
        let recorded_usage = reply.usage.map(|usage| ModelUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        });
        if let Err(error) = transcript.append(&Record::model_reply(
            &plan.run_id,
            turn,
            200,
            reply.content.clone(),
            recorded_calls,
            reply.finish_reason.clone(),
            recorded_usage,
            &reply.body,
        )) {
            return RunEnd::failed(format!("the transcript cannot be written: {error}"));
        }
        log.line(&format!("[turn {turn}] the model {}", reply.summary()));

        messages.push(Message::assistant(
            reply.content.clone(),
            reply.tool_calls.clone(),
        ));

        if reply.tool_calls.is_empty() {
            return RunEnd::completed(format!("the model finished after {turn} turn(s)"));
        }

        for call in &reply.tool_calls {
            tool_index += 1;
            let content = match run_model_tool(call, turn, tool_index, plan, transcript, log) {
                Ok(content) => content,
                Err(end) => return end,
            };
            messages.push(Message::tool(call.id.clone(), content));
        }
    }
}

/// Run one tool the model asked for, and return what is fed back to the model.
fn run_model_tool(
    call: &ToolCall,
    turn: u32,
    index: u32,
    plan: &RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) -> Result<String, RunEnd> {
    let workspace = plan.workspace.display().to_string();
    let command = match tool_command(call) {
        Ok(command) => command,
        Err(problem) => {
            // The model asked for something impossible. That is a tool result, not a
            // failed run: it is told why, and it may try again.
            log.line(&format!("[turn {turn} tool {index}] refused: {problem}"));
            if let Err(error) = transcript.append(&Record::tool_call(
                Some(turn),
                index,
                &call.function.name,
                "",
                &workspace,
            )) {
                return Err(RunEnd::failed(format!(
                    "the transcript cannot be written: {error}"
                )));
            }
            if let Err(error) = transcript.append(&Record::tool_result(
                Some(turn),
                index,
                &call.function.name,
                None,
                None,
                0,
                Some(problem.clone()),
            )) {
                return Err(RunEnd::failed(format!(
                    "the transcript cannot be written: {error}"
                )));
            }
            return Ok(problem);
        }
    };

    log.line(&format!("[turn {turn} tool {index}] {command}"));
    if let Err(error) = transcript.append(&Record::tool_call(
        Some(turn),
        index,
        model::SHELL_TOOL,
        &command,
        &workspace,
    )) {
        return Err(RunEnd::failed(format!(
            "the transcript cannot be written: {error}"
        )));
    }
    let started = Instant::now();
    let outcome = match run_shell(&command, plan, log) {
        Ok(outcome) => outcome,
        Err(reason) => {
            let _ = transcript.append(&Record::tool_result(
                Some(turn),
                index,
                model::SHELL_TOOL,
                None,
                None,
                started.elapsed().as_millis() as u64,
                None,
            ));
            return Err(RunEnd::failed(reason));
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    log.line(&format!(
        "[turn {turn} tool {index}] {} after {duration_ms} ms",
        describe_end(outcome.exit_code, outcome.signal)
    ));
    if let Err(error) = transcript.append(&Record::tool_result(
        Some(turn),
        index,
        model::SHELL_TOOL,
        outcome.exit_code,
        outcome.signal,
        duration_ms,
        Some(outcome.excerpt.clone()),
    )) {
        return Err(RunEnd::failed(format!(
            "the transcript cannot be written: {error}"
        )));
    }
    Ok(format!(
        "{}\n{}",
        describe_end(outcome.exit_code, outcome.signal),
        outcome.excerpt
    ))
}

/// The command a tool call asks for, or why it cannot be run.
fn tool_command(call: &ToolCall) -> Result<String, String> {
    if call.function.name != model::SHELL_TOOL {
        return Err(format!(
            "no tool named `{}` is declared; the only tool is `{}`",
            call.function.name,
            model::SHELL_TOOL
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(&call.function.arguments)
        .map_err(|error| format!("the arguments are not JSON: {error}"))?;
    let command = parsed
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "the arguments hold no string `command`".to_owned())?;
    if command.trim().is_empty() {
        return Err("the `command` is empty".to_owned());
    }
    Ok(command.to_owned())
}

/// The system message that sets an agent up. It names no secret and nothing
/// outside the run.
fn system_prompt(plan: &RunPlan) -> String {
    format!(
        "You are the agent `{agent}`, working in your own workspace at {workspace}.\n\
         You are given one instruction. Do the work by calling the `{tool}` tool, one \
         shell command at a time; each command runs with the workspace as its working \
         directory and its output is returned to you.\n\
         The workspace is a directory, not a sandbox: run only commands that serve the \
         instruction.\n\
         When the work is done, reply with a short summary and no tool call. You have at \
         most {max_turns} turns in this run.",
        agent = plan.agent,
        workspace = plan.workspace.display(),
        tool = model::SHELL_TOOL,
        max_turns = plan.max_turns,
    )
}

/// How one tool ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutcome {
    exit_code: Option<i32>,
    signal: Option<i32>,
    /// A bounded, redacted excerpt of the tool's output.
    excerpt: String,
}

/// Run one shell command inside the workspace, capturing its output.
///
/// The shell is started with the workspace as its working directory, and its output
/// is captured, redacted, written to the run's log and returned as an excerpt. That is
/// the whole intent: a tool is meant to work in the workspace. It is not confinement,
/// and the documentation says so.
fn run_shell(command: &str, plan: &RunPlan, log: &mut RunLog) -> Result<ToolOutcome, String> {
    let mut child_command = Command::new("/bin/sh");
    child_command
        .arg("-c")
        .arg(command)
        .current_dir(&plan.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GEWU_AGENT", &plan.agent)
        .env("GEWU_RUN_ID", &plan.run_id)
        .env("GEWU_WORKSPACE", &plan.workspace);
    // `git` in the workspace authenticates as the agent, and asks this same program
    // for the token through `GIT_ASKPASS`. The token is read from the credentials
    // file by the askpass process and handed over a pipe, so it is never an argument
    // to a command, a URL or a line in the log.
    if let Ok(askpass) = std::env::current_exe() {
        child_command
            .env("GIT_ASKPASS", askpass)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(crate::workspace::ASKPASS_CREDENTIALS, &plan.credentials)
            .env(crate::workspace::ASKPASS_AGENT, &plan.agent);
    }
    let mut child = child_command
        .spawn()
        .map_err(|error| format!("the command `{command}` cannot be started: {error}"))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("the command `{command}` has no output pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("the command `{command}` has no error pipe"))?;
    // One reader on its own thread, so a command that fills both pipes cannot deadlock.
    let reader = thread::spawn(move || read_bounded(&mut stdout));
    let (err_bytes, _err_truncated) = read_bounded(&mut stderr);
    let status = child
        .wait()
        .map_err(|error| format!("the command `{command}` cannot be waited for: {error}"))?;
    let (out_bytes, _out_truncated) = reader.join().unwrap_or_default();

    let mut text = String::from_utf8_lossy(&out_bytes).into_owned();
    if !err_bytes.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&err_bytes));
    }
    // Redact before the output is written anywhere or shown to the model.
    let redacted = log.secret().redact(&text);
    log.block(&redacted);
    Ok(ToolOutcome {
        exit_code: status.code(),
        signal: signal_of(status),
        excerpt: model::excerpt(&redacted, model::MAX_TOOL_OUTPUT),
    })
}

/// Read at most [`MAX_TOOL_OUTPUT_BYTES`] from one stream.
fn read_bounded(reader: &mut impl Read) -> (Vec<u8>, bool) {
    let limit = MAX_TOOL_OUTPUT_BYTES as u64;
    let mut bytes = Vec::new();
    let _ = reader.take(limit + 1).read_to_end(&mut bytes);
    let truncated = bytes.len() as u64 > limit;
    if truncated {
        bytes.truncate(limit as usize);
    }
    (bytes, truncated)
}

/// A tool's end, as a sentence.
fn describe_end(exit_code: Option<i32>, signal: Option<i32>) -> String {
    match (exit_code, signal) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("ended by signal {signal}"),
        (None, None) => "ended with no status".to_owned(),
    }
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
    use crate::test_stub::{Stub, StubReply};
    use crate::test_support::scratch_root;
    use std::path::PathBuf;

    /// A made-up key. It is not a credential and guards nothing; it exists so the
    /// tests can prove it never reaches a transcript, a log or an error.
    const TEST_KEY: &str = "gw-fixture-key-not-a-real-secret";

    /// A scratch agent: an instruction, a skill set, a workspace and a credentials file.
    fn agent_source(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = scratch_root(tag);
        let config = root.join("agents").join("scribe");
        let skills = config.join("skills");
        let workspace = config.join("workspace");
        fs::create_dir_all(&skills).expect("skills directory");
        fs::create_dir_all(&workspace).expect("workspace directory");
        let credentials = root.join("credentials.toml");
        crate::mode::write_private(
            &credentials,
            format!("[model_keys]\n\"scribe\" = \"{TEST_KEY}\"\n").as_bytes(),
        )
        .expect("the credentials file is written");
        (root, config, skills, credentials)
    }

    fn plan_with(
        run_dir: &Path,
        config: &Path,
        skills: &Path,
        credentials: &Path,
        endpoint: &str,
        max_turns: u32,
        steps: Vec<String>,
    ) -> RunPlan {
        let plan = RunPlan {
            run_id: "run-1".to_owned(),
            agent: "scribe".to_owned(),
            workspace: config.join("workspace"),
            instruction: config.join("instruction.md"),
            skills_dir: skills.to_path_buf(),
            endpoint: endpoint.to_owned(),
            model: "example-model".to_owned(),
            credentials: credentials.to_path_buf(),
            max_turns,
            steps,
            timeout_secs: 30,
        };
        plan.write(run_dir).expect("the plan is written");
        plan
    }

    fn completion(content: &str) -> String {
        serde_json::json!({
            "model": "example-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}
        })
        .to_string()
    }

    fn tool_call_completion(id: &str, command: &str) -> String {
        serde_json::json!({
            "model": "example-model",
            "choices": [{"index": 0, "finish_reason": "tool_calls",
                "message": {"role": "assistant", "content": null,
                    "tool_calls": [{"id": id, "type": "function",
                        "function": {"name": "shell",
                            "arguments": serde_json::json!({"command": command}).to_string()}}]}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}
        })
        .to_string()
    }

    fn run_dir(root: &Path) -> PathBuf {
        let dir = root.join("state").join("scribe").join("run-1");
        fs::create_dir_all(&dir).expect("run directory");
        dir
    }

    fn events(run_dir: &Path) -> Vec<String> {
        transcript::read(run_dir)
            .expect("the transcript reads")
            .records
            .iter()
            .map(|record| record.event().to_owned())
            .collect()
    }

    #[test]
    fn a_run_whose_model_finishes_succeeds_and_records_the_exchange() {
        let (root, config, skills, credentials) = agent_source("agent-run-finish");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "do nothing, carefully\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::json(completion("all done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");

        assert_eq!(
            events(&dir),
            vec![
                "run_started",
                "skills_discovered",
                "instruction_read",
                "run_limits",
                "model_request",
                "model_reply",
                "child_exit",
                "run_finished"
            ]
        );
        let file = transcript::read(&dir).expect("the transcript reads");
        let request = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ModelRequest {
                    turn,
                    authorization,
                    body,
                    ..
                } => Some((*turn, authorization.clone(), body.clone())),
                _ => None,
            })
            .expect("a request record");
        assert_eq!(request.0, 1);
        assert_eq!(request.1, "Bearer <redacted>");
        assert!(request.2.contains("do nothing, carefully"));
        assert!(request.2.contains("\"stream\":false"));
        assert!(!request.2.contains(TEST_KEY));
        let reply = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ModelReply { content, usage, .. } => Some((content.clone(), *usage)),
                _ => None,
            })
            .expect("a reply record");
        assert_eq!(reply.0.as_deref(), Some("all done"));
        assert_eq!(reply.1.map(|usage| usage.total_tokens), Some(11));
        // The bound is in the transcript and in the reason that ended the loop.
        let limits = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::RunLimits { max_turns, .. } => Some(*max_turns),
                _ => None,
            })
            .expect("a limits record");
        assert_eq!(limits, 4);
        let terminal = transcript::terminal_record(&dir).expect("a terminal record");
        assert!(matches!(
            terminal,
            Record::RunFinished { ref outcome, ref reason, .. }
                if outcome == "succeeded"
                    && reason.as_deref().unwrap_or("").contains("the model finished")
        ));
    }

    #[test]
    fn a_tool_call_round_trips_and_its_effect_is_in_the_workspace() {
        let (root, config, skills, credentials) = agent_source("agent-run-tool");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "make a note\n").expect("instruction");
        let stub = Stub::start(vec![
            StubReply::json(tool_call_completion(
                "call_1",
                "printf 'from the model\\n' > note.txt",
            )),
            StubReply::json(completion("wrote the note")),
        ]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");
        assert_eq!(
            fs::read_to_string(config.join("workspace").join("note.txt"))
                .expect("the tool wrote it"),
            "from the model\n"
        );

        // The transcript holds the tool call and its result, both tied to turn one.
        let file = transcript::read(&dir).expect("the transcript reads");
        let call = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ToolCall {
                    turn,
                    index,
                    command,
                    ..
                } => Some((*turn, *index, command.clone())),
                _ => None,
            })
            .expect("a tool call");
        assert_eq!(call.0, Some(1));
        assert_eq!(call.1, 1);
        assert_eq!(call.2, "printf 'from the model\\n' > note.txt");
        let result = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult {
                    turn,
                    exit_code,
                    output,
                    ..
                } => Some((*turn, *exit_code, output.clone())),
                _ => None,
            })
            .expect("a tool result");
        assert_eq!(result.0, Some(1));
        assert_eq!(result.1, Some(0));

        // Two exchanges: the tool call, then the finish.
        let requests = stub.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].body.contains("from the model"),
            "{}",
            requests[1].body
        );
        assert!(requests[1].body.contains("\"role\":\"tool\""));
    }

    #[test]
    fn the_declared_steps_run_before_the_model_and_are_marked_as_declared() {
        let (root, config, skills, credentials) = agent_source("agent-run-prelude");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "make a note\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            vec!["printf 'from the step\\n' > note.txt".to_owned()],
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");
        let file = transcript::read(&dir).expect("the transcript reads");
        let declared = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ToolCall { turn, command, .. } if command.contains("from the step") => {
                    Some(*turn)
                }
                _ => None,
            })
            .expect("the declared step");
        assert_eq!(declared, None, "a declared step has no model turn");
    }

    #[test]
    fn a_failing_declared_step_ends_the_run_with_that_code_before_any_model_call() {
        let (root, config, skills, credentials) = agent_source("agent-run-fail");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "fail\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            vec!["exit 7".to_owned(), "touch never-ran.txt".to_owned()],
        );

        let end = execute(&dir);
        assert_eq!(end.exit_code, 7);
        assert!(end.reason.expect("a reason").contains("step 1"));
        assert!(
            !config.join("workspace").join("never-ran.txt").exists(),
            "a run stops at the first failing step"
        );
        assert_eq!(stub.requests().len(), 0, "the model was never called");
    }

    #[test]
    fn reaching_the_turn_bound_ends_the_loop_with_the_bound_in_the_reason() {
        let (root, config, skills, credentials) = agent_source("agent-run-bound");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "keep going\n").expect("instruction");
        // Every reply asks for another tool call, so only the bound can stop the loop.
        let stub = Stub::start(vec![StubReply::json(tool_call_completion(
            "call_x", "true",
        ))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            2,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        let reason = end.reason.expect("a reason");
        assert!(reason.contains("maximum of 2 turn(s)"), "{reason}");
        assert!(reason.contains("max_turns"), "{reason}");
        assert_eq!(stub.requests().len(), 2, "the bound stopped the third call");
        assert!(matches!(
            transcript::terminal_record(&dir),
            Some(Record::RunFinished { ref reason, .. })
                if reason.as_deref().unwrap_or("").contains("max_turns")
        ));
    }

    #[test]
    fn a_malformed_reply_is_recorded_with_its_class_and_ends_the_run() {
        let (root, config, skills, credentials) = agent_source("agent-run-malformed");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "say something\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::json("not a chat completion")]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("malformed_reply"));
        let file = transcript::read(&dir).expect("the transcript reads");
        let class = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ModelCallFailed { class, .. } => Some(class.clone()),
                _ => None,
            })
            .expect("a failure record");
        assert_eq!(class, "malformed_reply");
    }

    #[test]
    fn a_refused_key_ends_the_run_with_the_bad_key_class() {
        let (root, config, skills, credentials) = agent_source("agent-run-bad-key");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "hello\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::status(401, r#"{"error":"no"}"#)]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("bad_key"));
        let file = transcript::read(&dir).expect("the transcript reads");
        assert!(file.records.iter().any(
            |record| matches!(record, Record::ModelCallFailed { class, .. } if class == "bad_key")
        ));
        // F-J: the failure sentence appears in the log exactly once, not twice.
        let log_text = fs::read_to_string(dir.join(crate::run::LOG_FILE)).expect("the log reads");
        assert_eq!(
            log_text.matches("the model call failed (bad_key)").count(),
            1,
            "{log_text}"
        );
    }

    #[test]
    fn a_missing_key_fails_before_any_tool_or_model_call() {
        let (root, config, skills, _credentials) = agent_source("agent-run-no-key");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "hello\n").expect("instruction");
        let missing = root.join("nothing.toml");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(&dir, &config, &skills, &missing, &stub.url(), 4, Vec::new());

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("model key"));
        assert_eq!(stub.requests().len(), 0);
        assert!(!events(&dir).contains(&"tool_call".to_owned()));
    }

    #[test]
    fn a_tool_call_to_an_undeclared_tool_is_answered_rather_than_fatal() {
        let (root, config, skills, credentials) = agent_source("agent-run-unknown-tool");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "use a tool\n").expect("instruction");
        let unknown = serde_json::json!({
            "choices": [{"finish_reason": "tool_calls",
                "message": {"role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_z", "type": "function",
                        "function": {"name": "teleport", "arguments": "{}"}}]}}]
        })
        .to_string();
        let stub = Stub::start(vec![
            StubReply::json(unknown),
            StubReply::json(completion("understood")),
        ]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");
        let file = transcript::read(&dir).expect("the transcript reads");
        let refused = file
            .records
            .iter()
            .find_map(|record| match record {
                Record::ToolResult {
                    output: Some(output),
                    ..
                } if output.contains("teleport") => Some(output.clone()),
                _ => None,
            })
            .expect("a refusal");
        assert!(refused.contains("no tool named"), "{refused}");
        assert_eq!(stub.requests().len(), 2);
        assert!(stub.requests()[1].body.contains("no tool named"));
    }

    #[test]
    fn no_secret_reaches_the_transcript_or_the_log_even_when_a_tool_prints_the_file() {
        let (root, config, skills, credentials) = agent_source("agent-run-secret");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "read the file\n").expect("instruction");
        // The model asks the shell to print the credentials file. The output contains
        // the key, and the run must still leave nothing behind that holds it.
        let cat = tool_call_completion("call_cat", &format!("cat {}", credentials.display()));
        let stub = Stub::start(vec![
            StubReply::json(cat),
            StubReply::json(completion("read it")),
        ]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");

        // The stub saw the key in the tool's output only in the request the model
        // received... no: the tool output is fed back to the model, redacted.
        let requests = stub.requests();
        assert!(
            !requests[1].body.contains(TEST_KEY),
            "the key was fed back to the model"
        );
        assert!(requests[1].body.contains("<redacted>"));

        // Nothing the run wrote holds the key.
        let transcript_text = fs::read_to_string(dir.join("transcript.jsonl")).expect("readable");
        assert!(
            !transcript_text.contains(TEST_KEY),
            "the transcript holds the key"
        );
        let log_text = fs::read_to_string(dir.join("run.log")).expect("readable");
        assert!(!log_text.contains(TEST_KEY), "the log holds the key");
        let plan_text = fs::read_to_string(dir.join("plan.json")).expect("readable");
        assert!(!plan_text.contains(TEST_KEY), "the plan holds the key");
    }

    #[test]
    fn a_model_reply_that_echoes_the_key_is_redacted_in_the_transcript() {
        let (root, config, skills, credentials) = agent_source("agent-run-echo");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "repeat it\n").expect("instruction");
        let stub = Stub::start(vec![StubReply::json(completion(&format!(
            "the key is {TEST_KEY}"
        )))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");
        let transcript_text = fs::read_to_string(dir.join("transcript.jsonl")).expect("readable");
        assert!(
            !transcript_text.contains(TEST_KEY),
            "the echoed key survived in the transcript"
        );
    }

    #[test]
    fn a_missing_instruction_fails_the_run_with_a_sentence() {
        let (root, config, skills, credentials) = agent_source("agent-run-no-instruction");
        let dir = run_dir(&root);
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
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
        let (root, config, skills, credentials) = agent_source("agent-run-skills");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let alpha = skills.join("alpha");
        fs::create_dir_all(&alpha).expect("skill directory");
        fs::write(alpha.join("SKILL.md"), "# alpha\n").expect("manifest");
        fs::write(alpha.join("notes.txt"), "more\n").expect("extra file");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(end.is_success(), "{end:?}");
        assert_eq!(
            fs::read_to_string(dir.join("skills/alpha/SKILL.md")).expect("installed"),
            "# alpha\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("skills/alpha/notes.txt")).expect("installed"),
            "more\n"
        );
        let file = transcript::read(&dir).expect("the transcript reads");
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
        let (root, config, skills, credentials) = agent_source("agent-run-bad-skill");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        fs::create_dir_all(skills.join("empty")).expect("skill directory");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("no SKILL.md"));
        assert!(!dir.join("skills").join("empty").exists());
    }

    #[test]
    fn an_unsafe_skill_name_refuses_the_run() {
        let (root, config, skills, credentials) = agent_source("agent-run-unsafe-skill");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let bad = skills.join("Bad Name");
        fs::create_dir_all(&bad).expect("skill directory");
        fs::write(bad.join("SKILL.md"), "# bad\n").expect("manifest");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("skill name"));
    }

    #[test]
    fn a_link_inside_a_skill_refuses_the_run() {
        let (root, config, skills, credentials) = agent_source("agent-run-link-skill");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "use the skill\n").expect("instruction");
        let alpha = skills.join("alpha");
        fs::create_dir_all(&alpha).expect("skill directory");
        fs::write(alpha.join("SKILL.md"), "# alpha\n").expect("manifest");
        std::os::unix::fs::symlink("/etc/passwd", alpha.join("outside"))
            .expect("a link is made for the test");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("link"));
    }

    #[test]
    fn an_empty_instruction_refuses_the_run() {
        let (root, config, skills, credentials) = agent_source("agent-run-empty-instruction");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "").expect("instruction");
        let stub = Stub::start(vec![StubReply::json(completion("done"))]);
        plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(end.reason.expect("a reason").contains("empty"));
    }

    #[test]
    fn a_run_that_has_less_time_left_than_the_default_times_out_and_says_so() {
        let (root, config, skills, credentials) = agent_source("agent-run-timeout");
        let dir = run_dir(&root);
        fs::write(config.join("instruction.md"), "wait\n").expect("instruction");
        let stub = Stub::start(vec![StubReply {
            status: 200,
            body: completion("too late"),
            retry_after_secs: None,
            delay: Duration::from_secs(30),
        }]);
        let mut plan = plan_with(
            &dir,
            &config,
            &skills,
            &credentials,
            &stub.url(),
            4,
            Vec::new(),
        );
        plan.timeout_secs = 2;
        plan.write(&dir).expect("the shorter plan is written");

        let end = execute(&dir);
        assert!(!end.is_success());
        assert!(
            end.reason.as_ref().expect("a reason").contains("timeout"),
            "{end:?}"
        );
    }
}
