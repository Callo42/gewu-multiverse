//! The native-program branch of one run.
//!
//! The product run process stays the supervisor: it prepares a private
//! configuration, applies the product's Landlock boundary to the real program, sends
//! the instruction on stdin, records the native JSON events, and writes the same
//! durable transcript ending as the built-in loop. The native program owns its model
//! turns, tools, retries and session; this module never reimplements them.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::credentials::{self, Secret};
use crate::harness::{Program, Protocol};
use crate::model;
use crate::run::NativePlan;
use crate::transcript::{Record, Transcript};

use super::{RunEnd, RunEnding, RunEvidence, RunLog, Shell};

/// The run's private native directory under the confined run temp.
struct NativeFiles {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    config: PathBuf,
    key: PathBuf,
    key_helper: PathBuf,
    program: PathBuf,
    install_root: PathBuf,
    skills: Vec<PathBuf>,
    mcp_state: Option<PathBuf>,
    mcp_socket: Option<PathBuf>,
}

/// A supervisor-owned MCP server outside the native Landlock writable roots.
struct McpServer {
    child: Child,
    socket: PathBuf,
    state: PathBuf,
}

impl McpServer {
    fn start(
        plan: &crate::run::RunPlan,
        shell: &Shell,
        temp: &Path,
        _native_root: &Path,
        exe: &Path,
    ) -> Result<Option<Self>, String> {
        if crate::run::read_forgejo_context(&shell.run_dir).is_none()
            || credentials::forgejo_agent_token(&plan.credentials, &plan.agent).is_err()
        {
            return Ok(None);
        }
        let state = temp.join("mcp-state");
        std::fs::create_dir_all(&state)
            .map_err(|error| format!("the MCP state directory cannot be created: {error}"))?;
        let _ = crate::mode::set_mode(&state, crate::mode::PRIVATE_DIR_MODE);
        let socket = state.join("mcp.sock");
        let _ = std::fs::remove_file(&socket);
        let child = Command::new(exe)
            .arg("--mcp-forgejo-serve")
            .arg(&socket)
            .arg(&state)
            .arg(&shell.run_dir)
            .arg(&plan.credentials)
            .arg(&plan.agent)
            .arg(&plan.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("the Forgejo MCP server cannot start: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                return Ok(Some(Self {
                    child,
                    socket,
                    state,
                }));
            }
            if !crate::run::process_alive(child.id()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        Err("the Forgejo MCP server did not become ready".to_owned())
    }

    fn state(&self) -> &Path {
        &self.state
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A run-private native helper. Dropping it closes the helper before records.
struct HelperProcess {
    child: Child,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Start a run-private OpenCode server, create a session at the selected
/// workspace so its MCP context is initialized, and wait for the supervisor's
/// six-tool bridge to be connected. The server and client share config/auth.
fn start_opencode_server(
    files: &NativeFiles,
    plan: &crate::run::RunPlan,
    exe: &Path,
    shell: &Shell,
) -> Result<Option<(HelperProcess, String, String)>, String> {
    let Some(socket_parent) = files.mcp_state.as_ref() else {
        return Ok(None);
    };
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("OpenCode's private port cannot be chosen: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("OpenCode's private port cannot be read: {error}"))?
        .port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}");
    let password = format!("gewu-{}-{}", plan.run_id, std::process::id());
    let spec = files.root.join("opencode-server.json");
    write_json(
        &spec,
        &json!({
            "workspace": plan.workspace.display().to_string(),
            "program": files.program.display().to_string(),
            "args": ["serve", "--hostname", "127.0.0.1", "--port", port.to_string()],
            "read": [files.install_root.display().to_string(), exe.display().to_string(), files.mcp_socket.as_ref().and_then(|p| p.parent()).map(|p| p.display().to_string()).unwrap_or_default(), shell.askpass.as_ref().map(|p| p.display().to_string()).unwrap_or_default()],
            "write": [files.root.display().to_string()],
        }),
    )?;
    let mut command = Command::new(exe);
    command
        .arg("--confined-program")
        .arg(&spec)
        .current_dir(&plan.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.env_clear();
    for (key, value) in crate::confinement::safe_environment(std::env::vars_os()) {
        command.env(key, value);
    }
    command
        .env("HOME", &files.home)
        .env("TMPDIR", &files.tmp)
        .env("XDG_CONFIG_HOME", files.root.join("opencode"))
        .env("XDG_STATE_HOME", files.config.join("state"))
        .env("XDG_CACHE_HOME", files.config.join("cache"))
        .env("XDG_RUNTIME_DIR", files.config.join("run"))
        .env("OPENCODE_PASSWORD", &password);
    if let Some(askpass) = shell.askpass.as_ref() {
        command
            .env("GIT_ASKPASS", exe)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(crate::workspace::ASKPASS_CREDENTIALS, askpass)
            .env(crate::workspace::ASKPASS_AGENT, &plan.agent);
        if let Some(login) = shell.login.as_ref() {
            command.env(crate::workspace::ASKPASS_LOGIN, login);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("OpenCode's private server cannot start: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut ready = false;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}")
                .parse()
                .expect("loopback address"),
            Duration::from_millis(100),
        )
        .is_ok()
        {
            ready = true;
            break;
        }
        if !crate::run::process_alive(child.id()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err("OpenCode's private server did not open its loopback port".to_owned());
    }
    std::thread::sleep(Duration::from_millis(800));
    let auth = format!(
        "Basic {}",
        base64(format!("opencode:{password}").as_bytes())
    );
    let http = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let body = json!({"title": "warm", "location": {"directory": plan.workspace}});
    let response = http
        .post(format!("{url}/api/session"))
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .send(serde_json::to_vec(&body).expect("the warm session body"));
    match response {
        Ok(reply) if reply.status().is_success() => {}
        Ok(reply) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "OpenCode's private session could not be created (HTTP {})",
                reply.status()
            ));
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "OpenCode's private session could not be created: {error}"
            ));
        }
    }
    let socket = files.mcp_socket.as_ref().ok_or_else(|| {
        "OpenCode's MCP socket is missing from the private launch files".to_owned()
    })?;
    let body = json!({"config": {
        "type": "local",
        "command": [files.key_helper.display().to_string(), "--mcp-forgejo-connect".to_owned(), socket.display().to_string()],
        "codemode": true,
        "timeout": {"startup": 20000, "catalog": 20000, "execution": 120000}
    }});
    let added = http
        .put(format!("{url}/api/experimental/mcp/gewu_forgejo"))
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .send(serde_json::to_vec(&body).expect("the MCP config body"));
    match added {
        Ok(reply) if reply.status().is_success() => {}
        Ok(reply) => {
            let status = reply.status();
            let list = http
                .get(format!("{url}/api/mcp"))
                .header("Authorization", &auth)
                .call()
                .ok()
                .and_then(|response| response.into_body().read_to_string().ok())
                .unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "OpenCode's Forgejo MCP server could not be added (HTTP {status}; MCP list: {list})"
            ));
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "OpenCode's Forgejo MCP server could not be added: {error}"
            ));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if socket_parent.join("tools-ready-6").is_file() {
            return Ok(Some((HelperProcess { child }, url, password)));
        }
        if !crate::run::process_alive(child.id()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("OpenCode's private server did not register all six MCP tools".to_owned())
}

/// One tool call still open in the native event stream.
#[derive(Debug, Clone)]
struct OpenCall {
    index: u32,
    tool: String,
    started: Instant,
}

/// Redact a known secret in raw, JSON-escaped and line-split forms before any
/// event or transcript persistence. Exact replacement is tried first; the escaped
/// JSON form covers a native program emitting a JSON string.
fn redact_native(secret: &Secret, text: &str) -> String {
    let mut out = secret.redact(text);
    if let Ok(quoted) = serde_json::to_string(secret.expose()) {
        let escaped = quoted.trim_matches('"');
        if escaped != secret.expose() && out.contains(escaped) {
            out = out.replace(escaped, "[redacted]");
        }
    }
    out
}

fn redact_native_line(secret: &Secret, line: &str, carry: &mut String) -> String {
    let combined = format!("{carry}{line}");
    let redacted = redact_native(secret, &combined);
    // Keep a bounded tail so a secret split across two native stdout lines is
    // removed when the second half arrives. Only the new suffix is persisted.
    *carry = combined
        .chars()
        .rev()
        .take(256)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let cut = redacted
        .len()
        .saturating_sub(line.len())
        .min(redacted.len());
    redacted[cut..].to_owned()
}

/// The facts the product reads from the native stream.
#[derive(Debug, Default)]
struct NativeState {
    final_text: Option<String>,
    terminal: bool,
    error: Option<String>,
    open: HashMap<String, OpenCall>,
    failed_call: Option<String>,
    refused_write: Option<String>,
    wrote: bool,
    workspace_wrote: bool,
    pr_ok: bool,
    comment_ok: bool,
    pushed: bool,
    uncertain_write: bool,
    tool_ok: bool,
    checked_empty: bool,
}

/// Run one native program. The caller has already read the instruction, key, skills
/// and declared steps; this function owns the program and its records.
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    plan: &crate::run::RunPlan,
    native: &NativePlan,
    instruction: &[u8],
    secret: &Secret,
    mut tool_index: u32,
    shell: &Shell,
    transcript: &mut Transcript,
    log: &mut RunLog,
) -> RunEnd {
    let Some(program) = Program::parse(&native.harness) else {
        return RunEnd::failed(format!(
            "the saved program `{}` is not one this product knows",
            native.harness
        ));
    };
    let Some(protocol) = Protocol::parse(&native.protocol) else {
        return RunEnd::failed(format!(
            "the saved connection type `{}` is not one this product knows",
            native.protocol
        ));
    };
    if let Some(error) = crate::harness::compatibility_error(program, protocol) {
        return RunEnd::failed(error);
    }
    if shell.method != crate::confinement::Method::Landlock {
        return RunEnd::failed(
            "native programs need the automatic file boundary; this computer does not have it"
                .to_owned(),
        );
    }
    let Some(temp) = shell.temp.as_ref() else {
        return RunEnd::failed(
            "native programs need the automatic file boundary; this computer does not have it"
                .to_owned(),
        );
    };

    let mut files = match prepare_files(plan, native, program, protocol, shell, temp) {
        Ok(files) => files,
        Err(reason) => return RunEnd::failed(reason),
    };
    log.line(&format!(
        "native program: {} {} at {}",
        program.label(),
        native.version,
        files.program.display()
    ));
    let mcp_server = match McpServer::start(plan, shell, temp, &files.root, &files.key_helper) {
        Ok(server) => server,
        Err(reason) => return RunEnd::failed(reason),
    };
    if let Some(server) = mcp_server.as_ref() {
        files.mcp_state = Some(server.state().to_path_buf());
        files.mcp_socket = Some(server.socket.clone());
        log.line(&format!("mcp bridge: {}", server.socket.display()));
    } else {
        log.line("mcp bridge: off — no Forgejo context or own token");
    }
    let _mcp_server = mcp_server;

    let workspace_before = workspace_fingerprint(&plan.workspace);
    let (mut argv, envs, config_note) = match build_launch(plan, native, program, protocol, &files)
    {
        Ok(launch) => launch,
        Err(reason) => return RunEnd::failed(reason),
    };
    log.line(&format!("native configuration: {config_note}"));

    let mut opencode_server = None;
    let mut opencode_password = None;
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return RunEnd::failed(format!("this program cannot be found: {error}")),
    };
    if program == Program::Opencode {
        match start_opencode_server(&files, plan, &exe, shell) {
            Ok(Some((server, url, password))) => {
                let mut adjusted = vec![
                    argv[0].clone(),
                    "run".to_owned(),
                    "--server".to_owned(),
                    url,
                ];
                adjusted.extend(
                    argv.iter()
                        .skip(2)
                        .filter(|arg| *arg != "--standalone")
                        .cloned(),
                );
                argv = adjusted;
                opencode_server = Some(server);
                opencode_password = Some(password);
                log.line("OpenCode: workspace session and MCP bridge ready");
            }
            Ok(None) => {}
            Err(reason) => return RunEnd::failed(reason),
        }
    }
    let _opencode_server = opencode_server;

    let spec = files.root.join("confined-program.json");
    if let Err(error) = write_json(
        &spec,
        &json!({
            "workspace": plan.workspace.display().to_string(),
            "program": argv.first().cloned().unwrap_or_default(),
            "args": argv.iter().skip(1).cloned().collect::<Vec<_>>(),
            "read": [
                files.install_root.display().to_string(),
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(|parent| parent.display().to_string()))
                    .unwrap_or_default(),
                shell.askpass.as_ref().map(|path| path.display().to_string()).unwrap_or_default(),
                files.mcp_socket.as_ref().and_then(|path| path.parent()).map(|path| path.display().to_string()).unwrap_or_default(),
            ],
            "write": [files.root.display().to_string()],
        }),
    ) {
        return RunEnd::failed(format!("the native launch plan cannot be written: {error}"));
    }

    let mut command = Command::new(&exe);
    command
        .arg("--confined-program")
        .arg(&spec)
        .current_dir(&plan.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_clear();
    for (key, value) in crate::confinement::safe_environment(std::env::vars_os()) {
        command.env(key, value);
    }
    command
        .env("HOME", &files.home)
        .env("TMPDIR", &files.tmp)
        .env("XDG_CONFIG_HOME", files.config.join("config"))
        .env("XDG_STATE_HOME", files.config.join("state"))
        .env("XDG_CACHE_HOME", files.config.join("cache"))
        .env("XDG_RUNTIME_DIR", files.config.join("run"))
        .env("GIT_CONFIG_GLOBAL", files.home.join(".gitconfig"))
        .env("GEWU_AGENT", &plan.agent)
        .env("GEWU_RUN_ID", &plan.run_id)
        .env("GEWU_WORKSPACE", &plan.workspace);
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(password) = opencode_password.as_deref() {
        command.env("OPENCODE_PASSWORD", password);
    }
    // The agent's own Forgejo token reaches git through the existing askpass route.
    if let Some(askpass) = shell.askpass.as_ref() {
        command
            .env("GIT_ASKPASS", exe.clone())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(crate::workspace::ASKPASS_CREDENTIALS, askpass)
            .env(crate::workspace::ASKPASS_AGENT, &plan.agent);
        if let Some(login) = shell.login.as_ref() {
            command.env(crate::workspace::ASKPASS_LOGIN, login);
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunEnd::failed(format!("{} could not be started: {error}", program.label()));
        }
    };
    let pid = child.id();
    if let Err(error) = crate::run::write_native_identity(shell.run_dir.as_path(), pid) {
        let _ = child.kill();
        let _ = child.wait();
        return RunEnd::failed(format!(
            "the native program started but its identity cannot be recorded: {error}"
        ));
    }
    if let Some(stdin) = child.stdin.as_mut() {
        if let Err(error) = stdin.write_all(instruction) {
            let _ = child.kill();
            let _ = child.wait();
            return RunEnd::failed(format!(
                "the instruction cannot be sent to the program: {error}"
            ));
        }
    }
    drop(child.stdin.take());

    let push_watch = crate::workspace::watch_push(&plan.workspace);
    let mut state = NativeState::default();
    let mut last_stderr = String::new();
    let mut event_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(crate::mode::SECRET_FILE_MODE)
        .open(files.root.join("events.jsonl"))
    {
        Ok(file) => file,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return RunEnd::failed(format!("the native event record cannot be opened: {error}"));
        }
    };

    let (tx, rx) = mpsc::channel::<(bool, String)>();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send((true, line));
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send((false, line));
            }
        });
    }
    drop(tx);

    let mut stdout_carry = String::new();
    let mut stderr_carry = String::new();
    let mut status = None;
    while status.is_none() {
        while let Ok((is_stdout, line)) = rx.try_recv() {
            if is_stdout {
                let cleaned = redact_native_line(secret, &line, &mut stdout_carry);
                if let Err(error) = writeln!(event_file, "{cleaned}") {
                    log.line(&format!(
                        "the native event record cannot be written: {error}"
                    ));
                }
                handle_event(
                    &cleaned,
                    program,
                    &mut state,
                    &mut tool_index,
                    plan,
                    transcript,
                    log,
                );
            } else {
                let cleaned = redact_native_line(secret, &line, &mut stderr_carry);
                if !cleaned.trim().is_empty() {
                    last_stderr = cleaned.clone();
                    log.block(&cleaned);
                }
            }
        }
        status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = child.kill();
                return RunEnd::failed(format!("the native program cannot be awaited: {error}"));
            }
        };
        if status.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Drain lines that were already in the pipe when the process exited.
    while let Ok((is_stdout, line)) = rx.recv_timeout(Duration::from_millis(200)) {
        let cleaned = if is_stdout {
            redact_native_line(secret, &line, &mut stdout_carry)
        } else {
            redact_native_line(secret, &line, &mut stderr_carry)
        };
        if is_stdout {
            let _ = writeln!(event_file, "{cleaned}");
            handle_event(
                &cleaned,
                program,
                &mut state,
                &mut tool_index,
                plan,
                transcript,
                log,
            );
        } else if !cleaned.trim().is_empty() {
            last_stderr = cleaned.clone();
            log.block(&cleaned);
        }
    }

    if let Some(pushed) = crate::workspace::pushed_during(&push_watch) {
        let repository = crate::workspace::remote_full_name(&pushed.repository);
        let base = crate::workspace::default_branch_of(&pushed.repository);
        let _ = crate::run::write_push_witness_at(
            &shell.run_dir,
            &pushed.branch,
            &pushed.remote_ref,
            repository.as_deref(),
            base.as_deref(),
        );
        state.wrote = true;
        state.pushed = true;
    }
    apply_ledger(&files, transcript, &mut state);
    if workspace_fingerprint(&plan.workspace) != workspace_before {
        state.wrote = true;
        state.workspace_wrote = true;
    }

    let exit = status.expect("the child exited");
    if let Some(call) = state.open.values().next().cloned() {
        finish_call(
            &call,
            1,
            None,
            "the program exited before this tool finished".to_owned(),
            transcript,
            log,
            &mut state,
        );
    }
    let reason = state
        .error
        .clone()
        .or_else(|| (!last_stderr.is_empty()).then_some(last_stderr))
        .or_else(|| {
            exit.signal()
                .map(|signal| format!("the program was ended by signal {signal}"))
        })
        .unwrap_or_else(|| format!("the program exited with code {}", exit.code().unwrap_or(1)));
    if !exit.success() {
        return RunEnd::failed(reason);
    }
    if !state.terminal {
        return RunEnd::failed(
            "the native program exited without a completion event; the run is not a success"
                .to_owned(),
        );
    }
    if let Some(error) = state.error.clone() {
        return RunEnd::failed(error);
    }

    let summary = state
        .final_text
        .as_deref()
        .map(|text| model::whole_message(text, 4000))
        .unwrap_or_default();
    let evidence = RunEvidence {
        failed_call: state.failed_call.clone(),
        refused_write: state.refused_write.clone(),
        wrote: state.wrote,
        checked_empty: state.checked_empty,
        ..RunEvidence::default()
    };
    let instruction = String::from_utf8_lossy(instruction);
    let base = crate::agent_run::ending_for_instruction(&instruction, &summary, &evidence);
    if state.uncertain_write {
        return RunEnd {
            exit_code: 0,
            reason: Some(summary),
            ending: RunEnding::Warned,
        };
    }
    let requested = requested_work(&instruction, &plan.steps);
    let ending = decide_native_ending(base, requested, &state);
    let reason = if summary.trim().is_empty() && matches!(ending, RunEnding::Warned) {
        Some(
            "the native program finished without a tool result; the result is not verified"
                .to_owned(),
        )
    } else {
        (!summary.trim().is_empty()).then_some(summary.clone())
    };
    RunEnd {
        exit_code: 0,
        reason,
        ending,
    }
}

/// Whether the instruction asks for a change, not only a report. This is a
/// bounded prose check: the tool/work witness below still decides whether the
/// change happened, and a read-only request may complete from a successful tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedWork {
    ReadOnly,
    GenericWrite,
    PullRequest,
    Comment,
    Ambiguous,
}

/// A bounded request classification for the native boundary. This is not a general
/// intent classifier: explicit read phrasing wins; explicit PR/comment/push asks
/// need the matching supervisor fact; only an ordinary change word may use a
/// workspace witness; everything else stays ambiguous and therefore warned.
fn requested_work(instruction: &str, steps: &[String]) -> RequestedWork {
    let lower = instruction.trim().to_ascii_lowercase();
    let read_phrasing = [
        "read ",
        "summarize",
        "summarise",
        "inspect ",
        "list ",
        "review ",
    ]
    .iter()
    .any(|word| lower.starts_with(word));
    let asks_pr = ["pull request", "open a pr", "open pr"]
        .iter()
        .any(|word| lower.contains(word));
    let asks_comment = lower.contains("comment")
        && (lower.contains("leave a comment")
            || lower.contains("post a comment")
            || lower.contains("write a comment")
            || lower.contains("add a comment"));
    let asks_push = lower.contains("push");
    if asks_pr || asks_comment || asks_push {
        if asks_pr {
            return RequestedWork::PullRequest;
        }
        if asks_comment {
            return RequestedWork::Comment;
        }
        return RequestedWork::GenericWrite;
    }
    if read_phrasing {
        return RequestedWork::ReadOnly;
    }
    let write_words = [
        "rename",
        "write",
        "edit",
        "fix",
        "implement",
        "modify",
        "delete",
        "remove",
        "create",
        "add ",
        "update",
    ];
    if write_words.iter().any(|word| lower.contains(word)) || !steps.is_empty() {
        return RequestedWork::GenericWrite;
    }
    if lower.is_empty() {
        RequestedWork::Ambiguous
    } else if lower.contains("read") || lower.contains("inspect") {
        RequestedWork::ReadOnly
    } else {
        RequestedWork::Ambiguous
    }
}

fn decide_native_ending(
    base: RunEnding,
    requested: RequestedWork,
    state: &NativeState,
) -> RunEnding {
    let base = if matches!(base, RunEnding::NoWork)
        && !(state.checked_empty && !state.wrote && state.failed_call.is_none())
    {
        RunEnding::Warned
    } else {
        base
    };
    match base {
        RunEnding::Succeeded => {
            let matched = match requested {
                RequestedWork::PullRequest => state.pr_ok && state.pushed,
                RequestedWork::Comment => state.comment_ok,
                RequestedWork::GenericWrite => state.workspace_wrote,
                RequestedWork::ReadOnly => state.tool_ok,
                RequestedWork::Ambiguous => false,
            };
            if matched {
                RunEnding::Succeeded
            } else {
                RunEnding::Warned
            }
        }
        other => other,
    }
}

/// A cheap before/after witness of actual workspace changes. It covers files the
/// native program itself created, edited or committed; arbitrary shell success with
/// no filesystem effect is not counted as a write.
fn workspace_fingerprint(root: &Path) -> std::collections::BTreeMap<PathBuf, (u64, u64)> {
    fn walk(root: &Path, path: &Path, out: &mut std::collections::BTreeMap<PathBuf, (u64, u64)>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                walk(root, &path, out);
            } else if kind.is_file() {
                let Ok(meta) = entry.metadata() else { continue };
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos() as u64)
                    .unwrap_or(0);
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                out.insert(relative, (meta.len(), modified));
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Prepare all private native files for one harness.
fn prepare_files(
    plan: &crate::run::RunPlan,
    native: &NativePlan,
    _program: Program,
    _protocol: Protocol,
    shell: &Shell,
    temp: &Path,
) -> Result<NativeFiles, String> {
    let root = temp.join("native");
    let home = root.join("home");
    let tmp = root.join("tmp");
    let config = root.join("config");
    for dir in [&root, &home, &tmp, &config] {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
        let _ = crate::mode::set_mode(dir, crate::mode::PRIVATE_DIR_MODE);
    }
    for name in ["config", "state", "cache", "run"] {
        let dir = config.join(name);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
        let _ = crate::mode::set_mode(&dir, crate::mode::PRIVATE_DIR_MODE);
    }
    let key = root.join("model.key");
    crate::mode::write_private(&key, plan_key(plan)?.as_bytes())
        .map_err(|error| format!("the run's key file cannot be written: {error}"))?;
    let exe = std::env::current_exe()
        .map_err(|error| format!("this program cannot be found: {error}"))?;
    let mut skills = Vec::new();
    let installed_skills = shell.run_dir.join("skills");
    if let Ok(entries) = std::fs::read_dir(&installed_skills) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                skills.push(entry.path());
            }
        }
    }
    skills.sort();
    Ok(NativeFiles {
        root,
        home,
        tmp,
        config,
        key,
        key_helper: exe,
        program: native.program.clone(),
        install_root: native.install_root.clone(),
        skills,
        mcp_state: None,
        mcp_socket: None,
    })
}

/// The resolved key for this exact agent.
fn plan_key(plan: &crate::run::RunPlan) -> Result<String, String> {
    credentials::model_key(&plan.credentials, &plan.agent)
        .map(|secret| secret.expose().to_owned())
        .map_err(|error| error.to_string())
}

/// Build the program command, its extra environment and a short config description.
type Launch = (Vec<String>, Vec<(String, String)>, String);

fn build_launch(
    plan: &crate::run::RunPlan,
    native: &NativePlan,
    program: Program,
    protocol: Protocol,
    files: &NativeFiles,
) -> Result<Launch, String> {
    let mut args: Vec<String> = Vec::new();
    let mut envs: Vec<(String, String)> = Vec::new();
    let mcp = files.mcp_socket.as_ref().map(|socket| {
        let exe = files.key_helper.display().to_string();
        (
            exe,
            vec![
                "--mcp-forgejo-connect".to_owned(),
                socket.display().to_string(),
            ],
        )
    });
    let model = native.model.clone();
    match program {
        Program::Pi => {
            let dir = files.root.join("pi");
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
            let _ = crate::mode::set_mode(&dir, crate::mode::PRIVATE_DIR_MODE);
            let api = match protocol {
                Protocol::ChatCompletions => "openai-completions",
                Protocol::Responses => "openai-responses",
                Protocol::AnthropicMessages => "anthropic-messages",
            };
            let models = json!({
                "providers": {
                    "gewu": {
                        "baseUrl": native.base_url,
                        "api": api,
                        "apiKey": format!("!{} --print-native-key {}", files.key_helper.display(), files.key.display()),
                        "models": [{"id": model, "name": model}]
                    }
                }
            });
            write_json(&dir.join("models.json"), &models)?;
            if let Some((exe, mcp_args)) = mcp.as_ref() {
                write_json(
                    &dir.join("mcp.json"),
                    &json!({"mcpServers": {"gewu_forgejo": {
                        "command": exe, "args": mcp_args, "exposure": "direct",
                        "description": "gewu-multiverse Forgejo tools for this agent"
                    }}}),
                )?;
            }
            envs.push(("PI_CODING_AGENT_DIR".to_owned(), dir.display().to_string()));
            envs.push(("PI_OFFLINE".to_owned(), "1".to_owned()));
            args.extend([
                "--mode".to_owned(),
                "json".to_owned(),
                "--no-session".to_owned(),
                "--provider".to_owned(),
                "gewu".to_owned(),
                "--model".to_owned(),
                model,
            ]);
            let skills_dir = dir.join("skills");
            for skill in &files.skills {
                let Some(name) = skill.file_name() else {
                    continue;
                };
                copy_tree(skill, &skills_dir.join(name))?;
                args.push("--skill".to_owned());
                args.push(skills_dir.join(name).display().to_string());
            }
        }
        Program::Codex => {
            let dir = files.root.join("codex");
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
            let _ = crate::mode::set_mode(&dir, crate::mode::PRIVATE_DIR_MODE);
            let sandbox = if native.codex_sandbox == "full-access" {
                "danger-full-access"
            } else {
                "workspace-write"
            };
            let mut config = format!(
                "model = {}\nmodel_provider = \"gewu\"\napproval_policy = \"never\"\nsandbox_mode = {}\n\n[sandbox_workspace_write]\nnetwork_access = true\n\n[features]\nplugins = false\nrecommended_plugins = false\n\n[model_providers.gewu]\nname = \"gewu\"\nbase_url = {}\nwire_api = \"responses\"\n\n[model_providers.gewu.auth]\ncommand = {}\nargs = [\"--print-native-key\", {}]\n",
                toml_string(&model),
                toml_string(sandbox),
                toml_string(&native.base_url),
                toml_string(&files.key_helper.display().to_string()),
                toml_string(&files.key.display().to_string())
            );
            if let Some((exe, mcp_args)) = mcp.as_ref() {
                config.push_str(&format!(
                    "\n[mcp_servers.gewu_forgejo]\ncommand = {}\nargs = [{}]\nrequired = true\nstartup_timeout_sec = 20\ntool_timeout_sec = 120\n",
                    toml_string(exe),
                    mcp_args
                        .iter()
                        .map(|arg| toml_string(arg))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            crate::mode::write_private(dir.join("config.toml"), config.as_bytes())
                .map_err(|error| format!("the Codex configuration cannot be written: {error}"))?;
            for skill in &files.skills {
                let Some(name) = skill.file_name() else {
                    continue;
                };
                copy_tree(skill, &dir.join("skills").join(name))?;
            }
            envs.push(("CODEX_HOME".to_owned(), dir.display().to_string()));
            args.extend([
                "exec".to_owned(),
                "--json".to_owned(),
                "--skip-git-repo-check".to_owned(),
                "--ephemeral".to_owned(),
                "-C".to_owned(),
                plan.workspace.display().to_string(),
                "--sandbox".to_owned(),
                sandbox.to_owned(),
            ]);
        }
        Program::Dsh => {
            let dir = files.root.join("dsh");
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("{} cannot be created: {error}", dir.display()))?;
            let _ = crate::mode::set_mode(&dir, crate::mode::PRIVATE_DIR_MODE);
            let api = match protocol {
                Protocol::ChatCompletions => "openai-completions",
                Protocol::Responses => "openai-responses",
                Protocol::AnthropicMessages => "anthropic-messages",
            };
            let mut overlay = format!(
                "- id: agent-default-model\n  config:\n    provider: gewu\n    model: {}\n- id: llm-pi-ai\n  config:\n    providers:\n      gewu:\n        apiKeyEnv: GEWU_NATIVE_KEY\n        api: {}\n        baseURL: {}\n        models:\n          - id: {}\n- id: session-title-llm\n  disabled: true\n",
                yaml(&model),
                yaml(api),
                yaml(&native.base_url),
                yaml(&model)
            );
            if let Some((exe, mcp_args)) = mcp.as_ref() {
                overlay.push_str(&format!(
                    "- insert:\n    - id: gewu-forgejo\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: gewu_forgejo\n        transport: stdio\n        command: {}\n        args: [{}]\n        failOnStartupError: true\n",
                    yaml(exe),
                    mcp_args.iter().map(|arg| yaml(arg)).collect::<Vec<_>>().join(", ")
                ));
            }
            crate::mode::write_private(dir.join("overlay.yml"), overlay.as_bytes()).map_err(
                |error| format!("the DeepSeek Harness configuration cannot be written: {error}"),
            )?;
            let key = std::fs::read_to_string(&files.key)
                .map_err(|error| format!("the captured run key cannot be read: {error}"))?;
            let dotenv = format!("GEWU_NATIVE_KEY={}\n", key.trim_end());
            crate::mode::write_private(dir.join(".env"), dotenv.as_bytes()).map_err(|error| {
                format!("the DeepSeek Harness key file cannot be written: {error}")
            })?;
            for skill in &files.skills {
                let Some(name) = skill.file_name() else {
                    continue;
                };
                copy_tree(skill, &dir.join("skills").join(name))?;
            }
            envs.push(("DSH_HOME".to_owned(), dir.display().to_string()));
            envs.push((
                "DSH_PERMISSION_MODE".to_owned(),
                "workspace-write".to_owned(),
            ));
            args.extend([
                "--profile".to_owned(),
                "headless".to_owned(),
                "--patch".to_owned(),
                dir.join("overlay.yml").display().to_string(),
                "--json".to_owned(),
            ]);
        }
        Program::Opencode => {
            let dir = files.root.join("opencode");
            let cfg = dir.join("opencode");
            std::fs::create_dir_all(&cfg)
                .map_err(|error| format!("{} cannot be created: {error}", error))?;
            let _ = crate::mode::set_mode(&dir, crate::mode::PRIVATE_DIR_MODE);
            let npm = match protocol {
                Protocol::ChatCompletions => "@ai-sdk/openai-compatible",
                Protocol::Responses => "@ai-sdk/openai",
                Protocol::AnthropicMessages => "@ai-sdk/anthropic",
            };
            let mut config = json!({
                "$schema": "https://opencode.ai/config.json",
                "provider": {
                    "gewu": {
                        "npm": npm,
                        "name": "gewu",
                        "options": {
                            "baseURL": native.base_url,
                            "apiKey": format!("{{file:{}}}", files.key.display())
                        },
                        "models": {
                            model.clone(): {"name": model.clone(), "limit": {"context": 32000, "output": 4096}}
                        }
                    }
                },
                "model": format!("gewu/{model}")
            });
            if let Some((exe, mcp_args)) = mcp.as_ref() {
                config["mcp"] = json!({"gewu_forgejo": {
                    "type": "local", "command": std::iter::once(exe.clone()).chain(mcp_args.iter().cloned()).collect::<Vec<_>>(), "enabled": true
                }});
            }
            write_json(&cfg.join("opencode.json"), &config)?;
            for skill in &files.skills {
                let Some(name) = skill.file_name() else {
                    continue;
                };
                copy_tree(skill, &cfg.join("skills").join(name))?;
            }
            envs.push(("XDG_CONFIG_HOME".to_owned(), dir.display().to_string()));
            args.extend([
                "run".to_owned(),
                "--format".to_owned(),
                "json".to_owned(),
                "--standalone".to_owned(),
                "--auto".to_owned(),
                "--model".to_owned(),
                format!("gewu/{model}"),
            ]);
        }
        Program::Builtin => return Err("the built-in engine is not a native program".to_owned()),
    }
    let mut argv = vec![files.program.display().to_string()];
    argv.extend(args);
    Ok((
        argv,
        envs,
        format!("{} on {}", program.label(), native.protocol),
    ))
}

/// Handle one native JSON event line.
fn handle_event(
    line: &str,
    program: Program,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match program {
        Program::Pi => pi_event(&value, state, tool_index, plan, transcript, log),
        Program::Codex => codex_event(&value, state, tool_index, plan, transcript, log),
        Program::Dsh => dsh_event(&value, state, tool_index, plan, transcript, log),
        Program::Opencode => opencode_event(&value, state, tool_index, plan, transcript, log),
        Program::Builtin => {}
    }
}

fn pi_event(
    value: &Value,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_execution_start") => start_call(
            value,
            "toolCallId",
            "toolName",
            "args",
            state,
            tool_index,
            plan,
            transcript,
            log,
        ),
        Some("tool_execution_end") => {
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let output = value
                .get("result")
                .and_then(|result| result.get("content"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let exit = value
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .and_then(|content| content.get("exit_code"))
                .and_then(Value::as_i64)
                .map(|code| code as i32)
                .or_else(|| {
                    value
                        .get("result")
                        .and_then(|result| result.get("isError"))
                        .or_else(|| value.get("isError"))
                        .and_then(Value::as_bool)
                        .filter(|error| *error)
                        .map(|_| 1)
                });
            finish_call_by_id(id, exit, output, state, transcript, log);
        }
        Some("message_end") => {
            if value
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
            {
                if let Some(text) = assistant_text(value.get("message")) {
                    state.final_text = Some(text.clone());
                    let _ = transcript.append(&Record::native_text(text));
                }
            }
        }
        Some("agent_settled") => {
            state.terminal = true;
            if value.get("aborted").and_then(Value::as_bool) == Some(true) {
                state.error = Some("the native program was stopped".to_owned());
            }
        }
        Some("error") => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the native program reported an error");
            state.error = Some(message.to_owned());
        }
        _ => {}
    }
}

fn codex_event(
    value: &Value,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("item.started") | Some("item.completed") => {
            let Some(item) = value.get("item") else {
                return;
            };
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
            match kind {
                "command_execution" => {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if value.get("type").and_then(Value::as_str) == Some("item.started") {
                        start_named(
                            id, "shell", command, state, tool_index, plan, transcript, log,
                        );
                    } else {
                        let exit = item
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .map(|code| code as i32);
                        let output = item
                            .get("aggregated_output")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        finish_call_by_id(id, exit, output, state, transcript, log);
                    }
                }
                "mcp_tool_call" | "mcp_call" => {
                    let id = item
                        .get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let tool = item
                        .get("tool")
                        .or_else(|| item.get("tool_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("forgejo");
                    let args = item
                        .get("arguments")
                        .or_else(|| item.get("args"))
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    if value.get("type").and_then(Value::as_str) == Some("item.started") {
                        start_named(id, tool, &args, state, tool_index, plan, transcript, log);
                    } else {
                        let status = item
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let output = item
                            .get("result")
                            .and_then(|result| result.get("content"))
                            .and_then(Value::as_array)
                            .and_then(|items| items.first())
                            .and_then(|item| item.get("text"))
                            .and_then(Value::as_str)
                            .or_else(|| item.get("output").and_then(Value::as_str))
                            .unwrap_or_default()
                            .to_owned();
                        let error = item
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let exit = error
                            .as_ref()
                            .map(|_| 1)
                            .or_else(|| (status == "failed").then_some(1));
                        let output = error.unwrap_or(output);
                        finish_call_by_id(id, exit, output, state, transcript, log);
                    }
                }
                "agent_message" => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        state.final_text = Some(text.to_owned());
                        let _ = transcript.append(&Record::native_text(text));
                    }
                }
                "error" => {
                    // "Model metadata ... not found" is a native warning, not a failed turn.
                    if let Some(text) = item.get("message").and_then(Value::as_str)
                        && !text.starts_with("Model metadata")
                    {
                        state.error = Some(text.to_owned());
                    }
                }
                _ => {}
            }
        }
        Some("error") => {
            if let Some(message) = value.get("message").and_then(Value::as_str) {
                state.error = Some(message.to_owned());
            }
        }
        Some("turn.completed") => state.terminal = true,
        Some("turn.failed") => {
            state.terminal = true;
            state
                .error
                .get_or_insert_with(|| "the native turn failed".to_owned());
        }
        _ => {}
    }
}

fn dsh_event(
    value: &Value,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_call") => {
            let id = value
                .get("callId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tool = value.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let args = value
                .get("input")
                .map(ToString::to_string)
                .unwrap_or_default();
            start_named(id, tool, &args, state, tool_index, plan, transcript, log);
        }
        Some("tool_result") => {
            let id = value
                .get("callId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let output = value
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            finish_call_by_id(
                id,
                (status != "completed").then_some(1),
                output,
                state,
                transcript,
                log,
            );
        }
        Some("text") => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                state.final_text = Some(text.to_owned());
                let _ = transcript.append(&Record::native_text(text));
            }
        }
        Some("final") => {
            state.terminal = true;
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    state.final_text = Some(text.to_owned());
                }
            }
            if let Some(reason) = value
                .get("reason")
                .and_then(|reason| reason.get("kind"))
                .and_then(Value::as_str)
                && reason != "completed"
            {
                state.error = Some(format!("the native run ended: {reason}"));
            }
        }
        Some("error") => {
            if let Some(message) = value.get("message").and_then(Value::as_str) {
                state.error = Some(message.to_owned());
            }
        }
        _ => {}
    }
}

fn opencode_event(
    value: &Value,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let Some(part) = value.get("part") else {
                return;
            };
            let id = part.get("id").and_then(Value::as_str).unwrap_or_default();
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let state_value = part.get("state");
            let status = state_value
                .and_then(|state| state.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            let args = state_value
                .and_then(|state| state.get("input"))
                .map(ToString::to_string)
                .unwrap_or_default();
            if !state.open.contains_key(id) {
                start_named(id, tool, &args, state, tool_index, plan, transcript, log);
            }
            if status == "completed" || status == "error" {
                let exit = state_value
                    .and_then(|state| state.get("metadata"))
                    .and_then(|metadata| metadata.get("metadata"))
                    .and_then(|metadata| metadata.get("exit"))
                    .and_then(Value::as_i64)
                    .map(|code| code as i32)
                    .or((status == "error").then_some(1));
                let output = state_value
                    .and_then(|state| state.get("output"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                finish_call_by_id(id, exit, output, state, transcript, log);
            }
        }
        Some("text") => {
            if let Some(text) = value
                .get("part")
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str)
            {
                state.final_text = Some(text.to_owned());
                // Only a text after the last tool and with no open call is a
                // completion; a later tool start resets this.
                state.terminal = state.open.is_empty();
                let _ = transcript.append(&Record::native_text(text));
            }
        }
        Some("error") => {
            if let Some(message) = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
            {
                state.error = Some(message.to_owned());
            } else if let Some(message) = value.get("message").and_then(Value::as_str) {
                state.error = Some(message.to_owned());
            }
        }
        Some("step_finish") => {
            let reason = value
                .get("part")
                .and_then(|part| part.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            log.line(&format!("opencode step_finish reason={reason}"));
            if matches!(reason, "stop" | "end-turn" | "complete") {
                state.terminal = true;
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn start_call(
    value: &Value,
    id_key: &str,
    tool_key: &str,
    args_key: &str,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    let id = value
        .get(id_key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool = value
        .get(tool_key)
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let args = value
        .get(args_key)
        .map(ToString::to_string)
        .unwrap_or_default();
    start_named(id, tool, &args, state, tool_index, plan, transcript, log);
}

#[allow(clippy::too_many_arguments)]
fn start_named(
    id: &str,
    tool: &str,
    args: &str,
    state: &mut NativeState,
    tool_index: &mut u32,
    plan: &crate::run::RunPlan,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    if id.is_empty() || state.open.contains_key(id) {
        return;
    }
    *tool_index += 1;
    state.terminal = false;
    let index = *tool_index;
    let args = model::excerpt(args, 2000);
    let _ = transcript.append(&Record::tool_call(
        None,
        index,
        tool,
        &args,
        &plan.workspace.display().to_string(),
    ));
    log.tool_started(tool);
    state.open.insert(
        id.to_owned(),
        OpenCall {
            index,
            tool: tool.to_owned(),
            started: Instant::now(),
        },
    );
}

fn finish_call_by_id(
    id: &str,
    exit: Option<i32>,
    output: String,
    state: &mut NativeState,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    let Some(call) = state.open.remove(id) else {
        return;
    };
    finish_call(
        &call,
        exit.unwrap_or(0),
        None,
        output,
        transcript,
        log,
        state,
    );
}

fn finish_call(
    call: &OpenCall,
    exit: i32,
    signal: Option<i32>,
    output: String,
    transcript: &mut Transcript,
    log: &mut RunLog,
    state: &mut NativeState,
) {
    let output = model::excerpt(&output, 8000);
    let duration = call.started.elapsed().as_millis() as u64;
    log.tool_finished();
    let failed = signal.is_some() || exit != 0;
    if !failed && !is_write_tool(&call.tool) {
        state.tool_ok = true;
    }
    if failed {
        let cause = if output.trim().is_empty() {
            format!("the native tool exited with code {exit}")
        } else {
            output
                .lines()
                .last()
                .unwrap_or("the native tool failed")
                .trim()
                .to_owned()
        };
        if is_write_tool(&call.tool) {
            state.refused_write = Some(format!("{}: {cause}", call.tool));
            log.tool_failure(&call.tool, &cause);
        } else if state.failed_call.is_none() {
            state.failed_call = Some(format!("{}: {cause}", call.tool));
            log.tool_failure(&call.tool, &cause);
        }
    }
    let _ = transcript.append(&Record::tool_result(
        None,
        call.index,
        &call.tool,
        Some(exit),
        signal,
        duration,
        Some(output),
    ));
}

fn is_write_tool(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    lower.contains("forgejo_comment")
        || lower.contains("forgejo_open_pull_request")
        || lower.contains("forgejo_update_pull_request")
        || matches!(
            lower.as_str(),
            "write" | "edit" | "apply_patch" | "patch" | "file_change"
        )
}

/// Read the MCP server's per-run ledger and record its facts.
fn apply_ledger(files: &NativeFiles, transcript: &mut Transcript, state: &mut NativeState) {
    let Some(dir) = files.mcp_state.as_ref() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(dir.join("calls.jsonl")) else {
        return;
    };
    let mut index = 0u32;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        index += 1;
        let tool = record.get("tool").and_then(Value::as_str).unwrap_or("");
        let repository = record.get("repository").and_then(Value::as_str);
        let number = record.get("number").and_then(Value::as_u64);
        let ok = record.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let outcome = record.get("outcome").and_then(Value::as_str).unwrap_or("");
        let action = record.get("action").and_then(Value::as_str);
        let _ = transcript.append(&Record::forgejo_result(
            0,
            index,
            tool,
            repository,
            number,
            if ok { "ok" } else { outcome },
            None,
            action,
        ));
        if tool.ends_with(crate::forgejo_tools::LIST_ASSIGNED_ISSUES)
            && ok
            && outcome
                .trim_start()
                .starts_with("no open issue is assigned")
        {
            // A successful empty assigned read is a no-work fact; it never erases
            // an earlier failed read or write.
            state.checked_empty = true;
        }
        if crate::forgejo_tools::is_write_tool(tool) {
            if ok {
                state.wrote = true;
                if tool.ends_with(crate::forgejo_tools::COMMENT) {
                    state.comment_ok = true;
                } else {
                    state.pr_ok = true;
                }
            } else if record
                .get("accepted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                // The server accepted the request (2xx) but its reply could not be
                // read: a real effect may exist, so this is warned/unknown, never a
                // clean refusal.
                state.uncertain_write = true;
            } else {
                state.refused_write = Some(format!("{tool}: {outcome}"));
            }
        }
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("{} cannot be written: {error}", path.display()))?;
    crate::mode::write_private(path, text.as_bytes())
        .map_err(|error| format!("{} cannot be written: {error}", path.display()))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|error| format!("{} cannot be created: {error}", dst.display()))?;
    let _ = crate::mode::set_mode(dst, crate::mode::PRIVATE_DIR_MODE);
    for entry in std::fs::read_dir(src)
        .map_err(|error| format!("{} cannot be read: {error}", src.display()))?
        .flatten()
    {
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
            crate::mode::write_private(&target, &bytes)
                .map_err(|error| format!("{} cannot be written: {error}", target.display()))?;
        }
    }
    Ok(())
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn assistant_text(message: Option<&Value>) -> Option<String> {
    let message = message?;
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        return (!text.trim().is_empty()).then(|| text.to_owned());
    }
    let content = message.get("content").and_then(Value::as_array)?;
    let mut text = String::new();
    for item in content {
        if let Some(part) = item.get("text").and_then(Value::as_str) {
            text.push_str(part);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_forgejo_write_tool_is_a_write_and_a_read_is_not() {
        for name in [
            "forgejo_comment",
            "forgejo_open_pull_request",
            "forgejo_update_pull_request",
            "mcp__gewu_forgejo__forgejo_comment",
            "mcp__gewu_forgejo__forgejo_open_pull_request",
            "mcp__gewu_forgejo__forgejo_update_pull_request",
        ] {
            assert!(is_write_tool(name), "{name}");
        }
        for name in [
            "bash",
            "shell",
            "exec_command",
            "forgejo_list_assigned_issues",
            "forgejo_read_issue_or_pull",
            "mcp__gewu_forgejo__forgejo_read_issue_or_pull",
        ] {
            assert!(!is_write_tool(name), "{name}");
        }
    }

    #[test]
    fn the_native_plan_round_trips_the_exact_facts() {
        let root = std::env::temp_dir().join(format!(
            "gewu-native-plan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("the fixture root");
        let plan = NativePlan {
            harness: "codex".to_owned(),
            version: "0.164.0".to_owned(),
            program: PathBuf::from("/opt/gewu/codex"),
            install_root: PathBuf::from("/opt/gewu"),
            protocol: "responses".to_owned(),
            base_url: "http://127.0.0.1:1/v1".to_owned(),
            model: "synthetic-model".to_owned(),
            codex_sandbox: "full-access".to_owned(),
        };
        plan.write(&root).expect("write");
        assert_eq!(
            crate::run::NativePlan::read(&root).expect("read"),
            Some(plan)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod native_proof_tests {
    use super::*;

    #[test]
    fn r1_prose_no_work_without_a_checked_empty_read_is_warned() {
        let state = NativeState::default();
        assert_eq!(
            decide_native_ending(RunEnding::NoWork, RequestedWork::ReadOnly, &state),
            RunEnding::Warned
        );
        let checked = NativeState {
            checked_empty: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::NoWork, RequestedWork::ReadOnly, &checked),
            RunEnding::NoWork
        );
    }

    #[test]
    fn r2_pr_and_comment_requests_need_their_own_supervisor_facts() {
        let local = NativeState {
            workspace_wrote: true,
            wrote: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::PullRequest, &local),
            RunEnding::Warned
        );
        assert_eq!(
            requested_work("Open a pull request and comment on the issue.", &[]),
            RequestedWork::PullRequest
        );
        assert_eq!(
            requested_work("Read the comment on the issue and summarize it.", &[]),
            RequestedWork::ReadOnly
        );
        let comment = NativeState {
            comment_ok: true,
            tool_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::Comment, &comment),
            RunEnding::Succeeded
        );
    }

    #[test]
    fn r7_native_redaction_covers_json_escaped_and_split_secrets() {
        let secret = Secret::new("sk-runtime-secret-value".to_owned());
        let escaped = serde_json::to_string(secret.expose()).expect("quoted");
        let escaped = escaped.trim_matches('"');
        let mut carry = String::new();
        let first = redact_native_line(&secret, "prefix sk-runtime-", &mut carry);
        let second = redact_native_line(&secret, "secret-value suffix", &mut carry);
        let text = format!("{first}{second}");
        assert!(!text.contains(secret.expose()), "{text}");
        let line = redact_native_line(&secret, &format!("value={escaped}"), &mut carry);
        assert!(!line.contains(escaped), "{line}");
    }

    #[test]
    fn r2_read_only_comment_succeeds_from_a_read_fact_and_generic_write_needs_a_witness() {
        assert_eq!(
            requested_work("Read the comment on the issue and summarize it.", &[]),
            RequestedWork::ReadOnly
        );
        let read = NativeState {
            tool_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::ReadOnly, &read),
            RunEnding::Succeeded
        );
        assert_eq!(
            requested_work("Rename the parser module.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::GenericWrite, &read),
            RunEnding::Warned
        );
    }
}
