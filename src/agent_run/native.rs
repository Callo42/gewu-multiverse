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

mod outcome;
use outcome::{Disposition, NativeEvent, NativeOutcome, SupervisorEnd};

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
            .stderr(Stdio::piped())
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
        let mut text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut text);
        }
        Err(format!(
            "the Forgejo MCP server did not become ready: {}",
            text.lines().next().unwrap_or("no diagnostic")
        ))
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
    args: String,
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
    let plain = secret.expose();
    let suffix_prefix = plain
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(plain.len()))
        .filter(|index| *index >= 4 && *index <= line.len())
        .any(|index| line.ends_with(&plain[..index]));
    if suffix_prefix {
        // Withhold the whole line until the following fragment can be checked.
        carry.push_str(line);
        return String::new();
    }
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
    let mut cut = redacted
        .len()
        .saturating_sub(line.len())
        .min(redacted.len());
    while cut < redacted.len() && !redacted.is_char_boundary(cut) {
        cut += 1;
    }
    redacted[cut..].to_owned()
}

/// The facts the product reads from the native stream.
#[derive(Debug)]
struct NativeState {
    /// The per-harness terminal-outcome model. It owns the terminal decision,
    /// the ordered failure timeline and the text that is safe to commit.
    outcome: NativeOutcome,
    open: HashMap<String, OpenCall>,
    failed_call: Option<String>,
    first_failure: Option<String>,
    refused_write: Option<String>,
    wrote: bool,
    workspace_wrote: bool,
    /// The local work the workspace comparison found, when it found any.
    local_work: crate::workspace::WorkspaceChanges,
    pr_ok: bool,
    pr_updated: bool,
    comment_ok: bool,
    forgejo_read_ok: bool,
    pushed: bool,
    uncertain_write: bool,
    tool_ok: bool,
    checked_empty: bool,
}

impl Default for NativeState {
    fn default() -> Self {
        Self::new(Program::Builtin)
    }
}

impl NativeState {
    fn new(program: Program) -> Self {
        Self {
            outcome: NativeOutcome::new(program),
            open: HashMap::new(),
            failed_call: None,
            first_failure: None,
            refused_write: None,
            wrote: false,
            workspace_wrote: false,
            local_work: crate::workspace::WorkspaceChanges::default(),
            pr_ok: false,
            pr_updated: false,
            comment_ok: false,
            forgejo_read_ok: false,
            pushed: false,
            uncertain_write: false,
            tool_ok: false,
            checked_empty: false,
        }
    }
}

fn codex_tool_approvals() -> String {
    let mut text = String::new();
    for tool in crate::forgejo_tools::TOOL_NAMES {
        text.push_str(&format!(
            "\n[mcp_servers.gewu_forgejo.tools.{tool}]\napproval_mode = \"approve\"\n"
        ));
    }
    text
}

fn non_success_reason(state: &NativeState, requested: RequestedWork) -> String {
    if let Some(terminal) = state.outcome.terminal_failure_reason() {
        return terminal;
    }
    if let Some(first) = state
        .outcome
        .first_tool_failure()
        .or(state.first_failure.as_ref())
    {
        return first.clone();
    }
    if let Some(refused) = state.refused_write.as_ref() {
        return refused.clone();
    }
    if let Some(failed) = state.failed_call.as_ref() {
        return failed.clone();
    }
    if state.uncertain_write {
        return "the Forgejo write request was accepted but its result could not be read; the effect is uncertain".to_owned();
    }
    if observed_work(state) {
        // The run did record local work, but the requested remote work is not
        // verified. State that directly; the denial sentences below would
        // contradict the "workspace changed" clause that precedes this reason.
        return "the requested remote work was not verified; the local change is recorded"
            .to_owned();
    }
    if !state.outcome.saw_tool() && requires_confirmed_tool(requested) {
        // Codex's in-run workspace sandbox can refuse a command without emitting a
        // JSON event at all; the refusal reaches only the model, not this stream.
        // With no supported event surface the product cannot name the denial, but
        // it must not claim success for work whose tool calls left no confirmed
        // result. Say exactly what is known.
        return match state.outcome.program() {
            Some(Program::Codex) => "the native program ended successfully but no tool call or workspace change was observed; Codex's own workspace sandbox can refuse a command without emitting an event, so the requested work is unconfirmed and this run is not a success".to_owned(),
            _ => "the native program ended successfully but no tool call or workspace change was observed, so the requested work is unconfirmed and this run is not a success".to_owned(),
        };
    }
    "the native program finished without verified success; no matching tool or work fact was recorded".to_owned()
}

fn requires_confirmed_tool(requested: RequestedWork) -> bool {
    matches!(
        requested,
        RequestedWork::ReadOnly
            | RequestedWork::GenericWrite
            | RequestedWork::PullRequest
            | RequestedWork::PullRequestUpdate
            | RequestedWork::Comment
            | RequestedWork::Push
            | RequestedWork::ForgejoRead
    )
}

/// The local work's own sentence, including the boolean-only fallback older tests
/// and records may carry.
fn local_reason(state: &NativeState) -> Option<String> {
    state.local_work.reason().or_else(|| {
        state
            .workspace_wrote
            .then(|| "workspace changed: a local change was recorded".to_owned())
    })
}

/// Whether the run recorded a failure cause worth putting beside its local work.
fn has_failure_cause(state: &NativeState) -> bool {
    state.outcome.terminal_failure_reason().is_some()
        || state.outcome.first_tool_failure().is_some()
        || state.first_failure.is_some()
        || state.refused_write.is_some()
        || state.failed_call.is_some()
        || state.uncertain_write
}

fn run_reason(
    ending: RunEnding,
    summary: &str,
    state: &NativeState,
    requested: RequestedWork,
) -> Option<String> {
    match ending {
        RunEnding::Succeeded => (!summary.trim().is_empty()).then(|| summary.to_owned()),
        RunEnding::NoWork => Some(if summary.trim().is_empty() {
            "there was no assigned work".to_owned()
        } else {
            summary.to_owned()
        }),
        RunEnding::Warned => {
            let local = local_reason(state);
            match local {
                Some(local) if has_failure_cause(state) => {
                    Some(format!("{local}; {}", non_success_reason(state, requested)))
                }
                Some(local) => Some(local),
                None => Some(non_success_reason(state, requested)),
            }
        }
        RunEnding::Refused => Some(non_success_reason(state, requested)),
    }
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
    let git_proxy =
        match crate::git_proxy::GitProxy::start(&shell.run_dir, &plan.agent, &plan.credentials) {
            Ok(proxy) => proxy,
            Err(reason) => return RunEnd::failed(reason),
        };
    log.line(if git_proxy.is_some() {
        "native Git: fixed supervisor transport"
    } else {
        "native Git: no supervisor transport"
    });
    if let Some(proxy) = git_proxy.as_ref() {
        let path = files.home.join(".gitconfig");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[http]\n\tproxy = ");
        text.push_str(&proxy.url());
        text.push('\n');
        if let Err(error) = crate::mode::write_private(&path, text.as_bytes()) {
            return RunEnd::failed(format!(
                "the native Git transport config cannot be written: {error}"
            ));
        }
    }

    let workspace_before = crate::workspace::snapshot_workspace(&plan.workspace);
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
    // The native program never receives the Forgejo token or an askpass path.
    // Its Git transport is a fixed-target loopback proxy owned by this supervisor.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let _git_proxy = git_proxy;

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
    let mut state = NativeState::new(program);
    let mut stderr_lines: Vec<String> = Vec::new();
    let mut progress_written = String::new();
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
                write_progress(shell, &state, &mut progress_written, log);
            } else {
                let cleaned = redact_native_line(secret, &line, &mut stderr_carry);
                if !cleaned.trim().is_empty() {
                    if stderr_lines.len() < 64 {
                        stderr_lines.push(cleaned.clone());
                    }
                    log.block(&cleaned);
                    // Codex's JSON stream is silent about a retry that recovers, but
                    // its own warning names the status. Surface that temporary fact
                    // while it is happening; it never decides the run's ending.
                    if state.outcome.observe_retry_text(&cleaned) {
                        write_progress(shell, &state, &mut progress_written, log);
                    }
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
            write_progress(shell, &state, &mut progress_written, log);
        } else if !cleaned.trim().is_empty() {
            if stderr_lines.len() < 64 {
                stderr_lines.push(cleaned.clone());
            }
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
    let local = crate::workspace::changes_since(&workspace_before, &plan.workspace);
    if !local.is_empty() {
        let _ = crate::run::write_local_work(&shell.run_dir, &local);
        state.wrote = true;
        state.workspace_wrote = true;
        state.local_work = local;
    }

    let exit = status.expect("the child exited");
    if let Some(call) = state.open.values().next().cloned() {
        finish_call(
            &call,
            Some(1),
            None,
            "the program exited before this tool finished".to_owned(),
            transcript,
            log,
            &mut state,
        );
    }
    let resolution = state.outcome.finish(exit.code(), SupervisorEnd::Natural);
    // The natural ending is the final progress state too. OpenCode has no terminal
    // event, so without this its last persisted state would remain "retrying".
    match resolution.disposition {
        Disposition::Failed => {
            if let Some(reason) = resolution.reason.as_deref() {
                let _ = crate::run::write_native_progress(
                    &shell.run_dir,
                    "failed",
                    reason,
                    true,
                    state.outcome.tool_warning().map(String::as_str),
                );
            }
        }
        Disposition::Succeeded => {
            let _ = crate::run::write_native_progress(
                &shell.run_dir,
                "succeeded",
                "",
                true,
                state.outcome.tool_warning().map(String::as_str),
            );
        }
        Disposition::Stopped | Disposition::TimedOut => {}
    }
    // Commit only the text the harness marked successful. Failed or retried
    // attempts never reach the durable transcript.
    for text in state.outcome.committed_texts() {
        let _ = transcript.append(&Record::native_text(text.clone()));
    }
    let stderr_reason = stderr_cause(&stderr_lines);
    let generic_exit = resolution.reason.as_deref().is_none_or(|reason| {
        reason.starts_with("the native program exited with code")
            || reason == "the native program was ended by a signal"
    });
    let reason = if generic_exit {
        stderr_reason
            .clone()
            .or_else(|| resolution.reason.clone())
            .or_else(|| {
                exit.signal()
                    .map(|signal| format!("the program was ended by signal {signal}"))
            })
            .unwrap_or_else(|| format!("the program exited with code {}", exit.code().unwrap_or(1)))
    } else {
        resolution
            .reason
            .clone()
            .or(stderr_reason)
            .unwrap_or_else(|| format!("the program exited with code {}", exit.code().unwrap_or(1)))
    };
    if !exit.success() {
        return RunEnd::failed(reason);
    }
    if resolution.disposition == Disposition::Failed {
        return RunEnd::failed(reason);
    }
    if resolution.disposition != Disposition::Succeeded {
        return RunEnd::failed(
            "the native program ended without a completion event; the run is not a success"
                .to_owned(),
        );
    }

    let summary = resolution
        .text
        .as_deref()
        .map(|text| model::whole_message(text, 4000))
        .unwrap_or_default();
    let evidence = RunEvidence {
        failed_call: state.failed_call.clone(),
        refused_write: state.refused_write.clone(),
        wrote: state.wrote,
        checked_empty: state.checked_empty,
        remote: state.pr_ok || state.pushed || state.comment_ok,
        read: (state.tool_ok || state.forgejo_read_ok) && state.local_work.is_empty(),
        ..RunEvidence::default()
    };
    let instruction = String::from_utf8_lossy(instruction);
    let requested = requested_work(&instruction, &plan.steps);
    // The transcript-derived facts in `execute` decide the final mark and work
    // line. This is the summary/failure base only: a declared step, a native tool
    // event and an early failure are all recorded in the transcript and are read by
    // the one facts function from there.
    let ending = if state.uncertain_write {
        RunEnding::Warned
    } else {
        crate::agent_run::ending_for_instruction(&instruction, &summary, &evidence)
    };
    let reason = run_reason(ending, &summary, &state, requested);
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
    PullRequestUpdate,
    Comment,
    UnsupportedForgejoWrite,
    Push,
    ForgejoRead,
    Ambiguous,
}

/// A bounded request classification for the native boundary. This is not a general
/// intent classifier: explicit read phrasing wins; explicit PR/comment/push asks
/// need the matching supervisor fact; only an ordinary change word may use a
/// workspace witness; everything else stays ambiguous and therefore warned.
fn requested_work(instruction: &str, steps: &[String]) -> RequestedWork {
    let lower = instruction.trim().to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let changes = tokens.iter().any(|word| {
        matches!(
            *word,
            "change"
                | "refactor"
                | "rename"
                | "rewrite"
                | "implement"
                | "modify"
                | "update"
                | "fix"
                | "create"
                | "delete"
                | "remove"
                | "add"
                | "write"
                | "edit"
                | "apply"
                | "close"
                | "reopen"
        )
    });
    let imperative_comment = tokens.first() == Some(&"comment")
        || lower.contains("leave a comment")
        || lower.contains("post a comment")
        || lower.contains("write a comment")
        || lower.contains("add a comment");
    let read_prefix = [
        "read ",
        "summarize",
        "summarise",
        "inspect ",
        "list ",
        "review ",
    ]
    .iter()
    .any(|word| lower.starts_with(word));
    let forgejo = ["issue", "pull request", "forgejo"]
        .iter()
        .any(|word| lower.contains(word));
    let update_pr = lower.contains("update the pull request")
        || lower.contains("update pull request")
        || lower.contains("update the pr")
        || lower.contains("update pr");
    let open_pr = lower.contains("open a pull request")
        || lower.starts_with("open a pr")
        || lower.contains("create a pull request")
        || lower.contains("open the pull request");
    if lower.contains("close the issue")
        || lower.contains("close issue")
        || lower.contains("reopen the issue")
        || lower.contains("reopen issue")
    {
        // The six supplied tools cannot close or reopen an issue. A read, a shell
        // success or a generic tool acknowledgement cannot satisfy this ask.
        return RequestedWork::UnsupportedForgejoWrite;
    }
    if imperative_comment {
        return RequestedWork::Comment;
    }
    let compound_action = read_prefix
        && tokens.iter().enumerate().any(|(index, word)| {
            if !matches!(*word, "and" | "then") {
                return false;
            }
            let mut next = index + 1;
            if tokens.get(next) == Some(&"then") {
                next += 1;
            }
            tokens.get(next).is_some_and(|word| {
                matches!(
                    *word,
                    "change"
                        | "refactor"
                        | "rename"
                        | "rewrite"
                        | "implement"
                        | "modify"
                        | "update"
                        | "fix"
                        | "create"
                        | "delete"
                        | "remove"
                        | "add"
                        | "write"
                        | "edit"
                        | "apply"
                        | "close"
                        | "reopen"
                        | "comment"
                        | "push"
                )
            })
        });
    if read_prefix && !compound_action && !update_pr && !open_pr {
        let path_like = lower.split_whitespace().any(|word| {
            word.contains('/')
                || [
                    ".rs", ".md", ".py", ".js", ".ts", ".go", ".java", ".c", ".h", ".toml",
                    ".json", ".yaml", ".yml", ".sh",
                ]
                .iter()
                .any(|suffix| word.ends_with(suffix))
        });
        let comment_local = tokens.contains(&"code")
            || tokens.contains(&"file")
            || tokens.contains(&"source")
            || path_like;
        let pr_explicit = tokens.contains(&"pr") || lower.contains("pull request");
        let comment_explicit = tokens.contains(&"comment") && !comment_local;
        let forgejo_noun = forgejo || pr_explicit || comment_explicit;
        return if forgejo_noun {
            RequestedWork::ForgejoRead
        } else {
            RequestedWork::ReadOnly
        };
    }
    if update_pr {
        return RequestedWork::PullRequestUpdate;
    }
    if open_pr {
        return RequestedWork::PullRequest;
    }
    if tokens.first() == Some(&"push") {
        return RequestedWork::Push;
    }
    if changes || !steps.is_empty() {
        return RequestedWork::GenericWrite;
    }
    if forgejo {
        return RequestedWork::ForgejoRead;
    }
    if tokens
        .iter()
        .any(|word| matches!(*word, "read" | "inspect"))
    {
        return RequestedWork::ReadOnly;
    }
    RequestedWork::Ambiguous
}

/// A successful piece of work the product itself observed: an accepted Forgejo
/// write (the MCP ledger's `ok` result), a Git push proved by the remote-tracking
/// reflog, or a real workspace change. It outranks the instruction classifier:
/// whatever the words asked for, real work is not a warning. This is what makes a
/// successful `forgejo_comment` or a verified push count for every native program,
/// including an instruction that also asked for something the tools cannot do
/// (`close the issue`) or that used a nominal change word (`do not change any files`).
fn observed_work(state: &NativeState) -> bool {
    state.wrote || state.comment_ok || state.pr_ok || state.pushed || state.workspace_wrote
}

/// The decision helper for the unit tests: the production path derives its facts
/// from the transcript in `execute`.
#[cfg(test)]
fn decide_native_ending(
    base: RunEnding,
    _requested: RequestedWork,
    state: &NativeState,
) -> RunEnding {
    let mut local = state.local_work.clone();
    if state.workspace_wrote && local.is_empty() {
        // Older tests set only the boolean. A changed workspace is still local.
        local.committed = true;
    }
    let facts = crate::run::RunFacts {
        remote: state.pr_ok || state.pushed || state.comment_ok,
        read: state.tool_ok || state.forgejo_read_ok,
        wrote: state.wrote,
        saw_tool: state.outcome.saw_tool(),
        local,
        ..crate::run::RunFacts::default()
    };
    super::decide_ending(base, &facts)
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

/// Per-launch request/retry settings that fit inside the session deadline.
///
/// The harness's own retry budget must be smaller than the product's session
/// deadline; otherwise a provider hang outlives the engine and is recorded only as
/// `timed_out`. Three attempts with a bounded request timeout and a margin leave the
/// harness time to emit its terminal event and its cause.
#[derive(Debug, Clone, Copy)]
struct LaunchBudget {
    request_ms: u64,
    retry_delay_ms: u64,
}

fn launch_budget(timeout_secs: u64) -> LaunchBudget {
    const ATTEMPTS: u64 = 3;
    let total_ms = timeout_secs.max(1).saturating_mul(1000);
    let usable = total_ms.saturating_sub(total_ms / 5);
    let request_ms = (usable / ATTEMPTS).clamp(1_000, 120_000);
    let retry_delay_ms = (request_ms / 10).clamp(200, 4_000);
    LaunchBudget {
        request_ms,
        retry_delay_ms,
    }
}

/// OpenCode 2.1.4 owns its retry loop and ignores the provider `maxRetries`
/// option, so its request timeout must fit an observed twelve attempts inside the
/// session deadline. Three attempts is the other harnesses' configured budget.
fn opencode_request_ms(timeout_secs: u64) -> u64 {
    const OBSERVED_ATTEMPTS: u64 = 12;
    let total_ms = timeout_secs.max(1).saturating_mul(1000);
    let usable = total_ms.saturating_sub(total_ms / 5);
    (usable / OBSERVED_ATTEMPTS).clamp(1_000, 45_000)
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
    let budget = launch_budget(plan.timeout_secs);
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
            write_json(
                &dir.join("settings.json"),
                &json!({
                    "httpIdleTimeoutMs": budget.request_ms,
                    "retry": {
                        "maxRetries": 2,
                        "baseDelayMs": budget.retry_delay_ms
                    }
                }),
            )?;
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
                "model = {}\nmodel_provider = \"gewu\"\napproval_policy = \"never\"\nsandbox_mode = {}\n\n[sandbox_workspace_write]\nnetwork_access = true\n\n[features]\nplugins = false\nrecommended_plugins = false\n\n[model_providers.gewu]\nname = \"gewu\"\nbase_url = {}\nwire_api = \"responses\"\nrequest_max_retries = 2\nstream_max_retries = 2\nstream_idle_timeout_ms = {}\n\n[model_providers.gewu.auth]\ncommand = {}\nargs = [\"--print-native-key\", {}]\n",
                toml_string(&model),
                toml_string(sandbox),
                toml_string(&native.base_url),
                budget.request_ms,
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
                config.push_str(&codex_tool_approvals());
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
            // The warning is the only place Codex names a recovered 503; without it
            // the card would show `working` in silence until the retry finished.
            envs.push((
                "RUST_LOG".to_owned(),
                "codex_core=warn,codex_otel=info".to_owned(),
            ));
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
                "- id: agent-default-model\n  config:\n    provider: gewu\n    model: {}\n- id: llm-pi-ai\n  config:\n    providers:\n      gewu:\n        apiKeyEnv: GEWU_NATIVE_KEY\n        api: {}\n        baseURL: {}\n        timeoutMs: {}\n        streamIdleTimeoutMs: {}\n        retryPolicy:\n          mode: normal\n          maxRetries: 2\n          backoff:\n            initialDelayMs: {}\n            maxDelayMs: {}\n        models:\n          - id: {}\n- id: session-title-llm\n  disabled: true\n",
                yaml(&model),
                yaml(api),
                yaml(&native.base_url),
                budget.request_ms,
                budget.request_ms,
                budget.retry_delay_ms,
                budget.retry_delay_ms.saturating_mul(4),
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
                            "apiKey": format!("{{file:{}}}", files.key.display()),
                            "timeout": opencode_request_ms(plan.timeout_secs),
                            "maxRetries": 2
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
    for event in outcome::line_events(program, &value) {
        match event {
            NativeEvent::ToolStarted { id, tool, args } => {
                state.outcome.observe(NativeEvent::ToolStarted {
                    id: id.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                });
                start_named(&id, &tool, &args, state, tool_index, plan, transcript, log);
            }
            NativeEvent::ToolFinished {
                id,
                tool,
                exit,
                signal,
                output,
            } => {
                // The live transcript owns completion; the outcome model records
                // the result once in finish_call, through the same function the
                // replay corpus uses.
                state.outcome.forget_tool(&id);
                let id = if id.is_empty() {
                    format!("native-{}", *tool_index + 1)
                } else {
                    id
                };
                if !state.open.contains_key(&id) {
                    let name = tool.unwrap_or_else(|| "tool".to_owned());
                    start_named(&id, &name, "", state, tool_index, plan, transcript, log);
                }
                finish_call_by_id(&id, exit, signal, output, state, transcript, log);
            }
            other => state.outcome.observe(other),
        }
    }
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
    let index = *tool_index;
    let excerpt = model::excerpt(args, 2000);
    let _ = transcript.append(&Record::tool_call(
        None,
        index,
        tool,
        &excerpt,
        &plan.workspace.display().to_string(),
    ));
    log.tool_started(tool);
    state.open.insert(
        id.to_owned(),
        OpenCall {
            index,
            tool: tool.to_owned(),
            args: args.to_owned(),
            started: Instant::now(),
        },
    );
}

fn finish_call_by_id(
    id: &str,
    exit: Option<i32>,
    signal: Option<String>,
    output: String,
    state: &mut NativeState,
    transcript: &mut Transcript,
    log: &mut RunLog,
) {
    let Some(call) = state.open.remove(id) else {
        return;
    };
    finish_call(&call, exit, signal, output, transcript, log, state);
}

fn finish_call(
    call: &OpenCall,
    exit: Option<i32>,
    signal: Option<String>,
    output: String,
    transcript: &mut Transcript,
    log: &mut RunLog,
    state: &mut NativeState,
) {
    let stored_output = model::excerpt(&output, 8000);
    let duration = call.started.elapsed().as_millis() as u64;
    log.tool_finished();
    let diagnostic =
        state
            .outcome
            .record_tool(&call.tool, &call.args, exit, signal.clone(), &output);
    if let Some(diagnostic) = diagnostic {
        let detail = diagnostic
            .strip_prefix(&format!("{}: ", call.tool))
            .unwrap_or(&diagnostic)
            .to_owned();
        if state.first_failure.is_none() {
            state.first_failure = Some(diagnostic.clone());
        }
        if is_write_tool(&call.tool) {
            state.refused_write = Some(diagnostic);
        } else if state.failed_call.is_none() {
            state.failed_call = Some(diagnostic);
        }
        log.tool_failure(&call.tool, &detail);
    } else if !is_write_tool(&call.tool) {
        state.tool_ok = true;
    }
    let signal_code = signal.as_deref().and_then(signal_number);
    let _ = transcript.append(&Record::tool_result(
        None,
        call.index,
        &call.tool,
        exit.or(Some(0)),
        signal_code,
        duration,
        Some(stored_output),
    ));
}

fn signal_number(name: &str) -> Option<i32> {
    let name = name.trim().to_ascii_uppercase();
    Some(match name.as_str() {
        "SIGHUP" => 1,
        "SIGINT" => 2,
        "SIGQUIT" => 3,
        "SIGKILL" => 9,
        "SIGSEGV" => 11,
        "SIGPIPE" => 13,
        "SIGTERM" => 15,
        _ => name.strip_prefix("SIG")?.parse().ok()?,
    })
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
        let url = record.get("url").and_then(Value::as_str);
        let branch = record.get("branch").and_then(Value::as_str);
        // The ledger now carries the reply's own number, URL and branch. Older
        // ledgers that only carried the call's arguments leave them absent, and the
        // work line then makes no claim it cannot prove.
        let _ = transcript.append(&Record::forgejo_result_with_branch(
            0,
            index,
            tool,
            repository,
            number,
            if ok { "ok" } else { outcome },
            url,
            action,
            branch,
        ));
        if ok && !crate::forgejo_tools::is_write_tool(tool) {
            state.forgejo_read_ok = true;
        }
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
            if ok && action == Some("already_open") {
                // The agent's own request is already open. The transcript names it
                // for the work block only when this run also pushed its branch or
                // commented on it; with no other work it stays a refused-write mark.
                state.refused_write = Some(format!("{tool}: {outcome}"));
            } else if ok {
                state.wrote = true;
                if tool.ends_with(crate::forgejo_tools::COMMENT) {
                    state.comment_ok = true;
                } else {
                    state.pr_ok = true;
                    if tool.ends_with(crate::forgejo_tools::UPDATE_PULL_REQUEST) {
                        state.pr_updated = true;
                    }
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

/// The first error sentence in a harness's stderr, with a bounded continuation.
///
/// The old adapter reported only the last non-empty line: Codex's TOML error became
/// the caret line and DSH's parse failure became `Node.js v24.19.0`. A startup
/// error's first sentence is the cause; stack frames and source markers are not.
fn stderr_cause(lines: &[String]) -> Option<String> {
    let index = lines.iter().position(|line| {
        let line = line.trim_start();
        line.starts_with("Error: ")
            || line.starts_with("Error ")
            || line.starts_with("error: ")
            || line.to_ascii_lowercase().contains("error:")
    })?;
    let mut out = lines[index].trim().to_owned();
    for line in lines.iter().skip(index + 1).take(4) {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("at ")
            || line.starts_with("Node.js ")
            || line.starts_with('^')
            || line.starts_with('|')
            || line.starts_with("  ")
        {
            break;
        }
        if out.ends_with(':') {
            out.push(' ');
        } else {
            out.push_str(": ");
        }
        out.push_str(line);
    }
    Some(out)
}

/// Persist the failure model's latest state beside the run, so the engine can
/// include the last emitted cause in a stop or deadline record even after the
/// native child is gone.
fn write_progress(shell: &Shell, state: &NativeState, last: &mut String, log: &mut RunLog) {
    let Some((progress_state, cause, terminal, warning)) = state.outcome.progress() else {
        return;
    };
    let signature = format!(
        "{progress_state}\u{0}{cause}\u{0}{terminal}\u{0}{}",
        warning.as_deref().unwrap_or_default()
    );
    if signature == *last {
        return;
    }
    *last = signature;
    log.line(&format!(
        "native progress: {progress_state}{}{}",
        if cause.is_empty() { "" } else { " — " },
        cause
    ));
    if let Err(error) = crate::run::write_native_progress(
        &shell.run_dir,
        &progress_state,
        &cause,
        terminal,
        warning.as_deref(),
    ) {
        log.line(&format!(
            "the native progress record cannot be written: {error}"
        ));
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
    fn r1_a_read_only_cycle_that_writes_nothing_is_no_work() {
        // Wave 37 C1: `nothing new to add` is the neutral outcome the README
        // promises for a check that read its issue and wrote nothing. A missing
        // empty-list ledger is not evidence that the model's own sentence is false.
        let state = NativeState::default();
        assert_eq!(
            decide_native_ending(RunEnding::NoWork, RequestedWork::ReadOnly, &state),
            RunEnding::NoWork
        );
        let checked = NativeState {
            checked_empty: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::NoWork, RequestedWork::ReadOnly, &checked),
            RunEnding::NoWork
        );
        assert_eq!(
            run_reason(
                RunEnding::NoWork,
                "nothing new to add",
                &state,
                RequestedWork::ReadOnly
            )
            .as_deref(),
            Some("nothing new to add")
        );
    }

    #[test]
    fn c1_observed_work_outranks_a_failed_test_and_a_negative_summary() {
        // The work is the product's own fact. A failing test before the fix and a
        // final sentence that claims failure cannot turn a pushed branch or an
        // opened pull request into a warning.
        let work = NativeState {
            pushed: true,
            workspace_wrote: true,
            first_failure: Some("shell: exit 1 — test failed".to_owned()),
            failed_call: Some("shell: exit 1 — test failed".to_owned()),
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Warned, RequestedWork::PullRequest, &work),
            RunEnding::Succeeded
        );
        let comment = NativeState {
            comment_ok: true,
            wrote: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Refused, RequestedWork::Comment, &comment),
            RunEnding::Succeeded
        );
        // The rejected checkout of a branch an interrupted run left behind is a
        // failed command, not a failed run, once the retry pushed and opened work.
        let existing_branch = NativeState {
            pushed: true,
            workspace_wrote: true,
            failed_call: Some(
                "bash: exit 128 — fatal: a branch named 'fix-2' already exists".to_owned(),
            ),
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(
                RunEnding::Warned,
                RequestedWork::PullRequest,
                &existing_branch
            ),
            RunEnding::Succeeded
        );
        // F1: a local edit/commit with a failed push is not verified remote work.
        // The failed command and the negative summary stay visible in the warning;
        // the reason names the local work rather than pretending it succeeded.
        let local_only = NativeState {
            workspace_wrote: true,
            failed_call: Some(
                "bash: exit 1 — error: failed to push some refs to 'nowhere'".to_owned(),
            ),
            ..NativeState::default()
        };
        let ending =
            decide_native_ending(RunEnding::Warned, RequestedWork::PullRequest, &local_only);
        assert_eq!(ending, RunEnding::Warned);
        let reason = run_reason(
            ending,
            "I could not finish the task. The work is still pending.",
            &local_only,
            RequestedWork::PullRequest,
        )
        .expect("reason");
        assert!(reason.contains("workspace changed"), "{reason}");
        assert!(reason.contains("failed to push"), "{reason}");
        assert!(
            !reason.contains("no matching tool or work fact"),
            "{reason}"
        );

        // A blocked write is not hidden; the reason names the observed work first.
        let blocked = NativeState {
            comment_ok: true,
            wrote: true,
            refused_write: Some("forgejo_update_pull_request: no open pull request".to_owned()),
            ..NativeState::default()
        };
        // The comment is real remote work, so the failed update cannot make the run
        // a refusal; the mark is a tick and the work block names the comment.
        let ending = decide_native_ending(
            RunEnding::Refused,
            RequestedWork::PullRequestUpdate,
            &blocked,
        );
        assert_eq!(ending, RunEnding::Succeeded);
        let reason = run_reason(
            ending,
            "I could not update the pull request",
            &blocked,
            RequestedWork::PullRequestUpdate,
        )
        .expect("reason");
        assert_eq!(reason, "I could not update the pull request");
    }

    #[test]
    fn r2_pr_and_comment_requests_need_their_own_supervisor_facts() {
        let local = NativeState {
            workspace_wrote: true,
            wrote: true,
            ..NativeState::default()
        };
        let local_ending =
            decide_native_ending(RunEnding::Succeeded, RequestedWork::PullRequest, &local);
        assert_eq!(
            local_ending,
            RunEnding::Warned,
            "a local-only write is not verified remote work"
        );
        let local_reason = run_reason(
            local_ending,
            "I could not push the branch.",
            &local,
            RequestedWork::PullRequest,
        )
        .expect("reason");
        assert!(local_reason.contains("workspace changed"), "{local_reason}");
        assert!(
            !local_reason.contains("no tool call or workspace change"),
            "{local_reason}"
        );
        assert!(
            !local_reason.contains("no matching tool or work fact"),
            "{local_reason}"
        );
        assert_eq!(
            requested_work("Open a pull request and comment on the issue.", &[]),
            RequestedWork::PullRequest
        );
        assert_eq!(
            requested_work("Read the comment on the issue and summarize it.", &[]),
            RequestedWork::ForgejoRead
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
    fn c2_imperative_comment_change_and_push_use_their_own_facts() {
        assert_eq!(
            requested_work("Comment on the issue.", &[]),
            RequestedWork::Comment
        );
        assert_eq!(
            requested_work("Review and change the form.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(requested_work("Push the branch.", &[]), RequestedWork::Push);
        let pushed = NativeState {
            pushed: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::Push, &pushed),
            RunEnding::Succeeded
        );
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::GenericWrite, &pushed),
            RunEnding::Succeeded
        );
    }

    /// Regression for wave 36 C1: the instruction's prose classifier is not the
    /// work witness. A successful MCP comment and a verified push are work even when
    /// the instruction also says `close the issue` (which the tools cannot do) or
    /// uses a nominal change word like `do not change any files`.
    #[test]
    fn c1_a_successful_comment_or_push_is_work_whatever_the_prose_asked() {
        let close = requested_work(
            "Look at the issue assigned to you. Comment on it with a plan, add a file, commit and push it, and close the issue.",
            &[],
        );
        assert_eq!(close, RequestedWork::UnsupportedForgejoWrite);
        let comment = NativeState {
            comment_ok: true,
            wrote: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, close, &comment),
            RunEnding::Succeeded,
            "a comment the product observed was reported as no work"
        );
        let push = NativeState {
            pushed: true,
            wrote: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, close, &push),
            RunEnding::Succeeded,
            "a verified push was reported as no work"
        );
        let do_not_change = requested_work(
            "Answer every new issue assigned to you. Leave one short, helpful comment. Do not change any files.",
            &[],
        );
        assert_eq!(do_not_change, RequestedWork::GenericWrite);
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, do_not_change, &comment),
            RunEnding::Succeeded,
            "the nominal `change` word hid the observed comment"
        );
        let no_work = NativeState::default();
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, close, &no_work),
            RunEnding::Warned,
            "with no recorded fact the requested work is unconfirmed"
        );
    }

    #[test]
    fn c3_forgejo_read_needs_the_supervisor_ledger() {
        let native_only = NativeState {
            tool_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(
                RunEnding::Succeeded,
                RequestedWork::ForgejoRead,
                &native_only
            ),
            RunEnding::NoWork,
            "a successful read with no write is no work"
        );
        let ledger = NativeState {
            forgejo_read_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::ForgejoRead, &ledger),
            RunEnding::NoWork,
            "a successful Forgejo read with no write is no work"
        );
    }

    #[test]
    fn r8_a_ledger_existing_request_is_named_but_is_not_this_runs_write() {
        // Walk A F3: the agent's own request is already open. The work block names
        // it, but the fact alone is not a write this run made. With no push or
        // comment the refused-write mark stands; a push in the same run outranks it.
        let root = crate::test_support::scratch_root("ledger-existing-pr");
        let run_dir = root.join("run-1700000000008-0001");
        std::fs::create_dir_all(&run_dir).expect("the run directory");
        let state = root.join("mcp-state");
        std::fs::create_dir_all(&state).expect("the MCP state");
        let line = serde_json::json!({
            "tool": crate::forgejo_tools::OPEN_PULL_REQUEST,
            "repository": "team/workspace",
            "number": 4,
            "ok": true,
            "outcome": "the pull request is already open",
            "action": "already_open",
            "url": "http://127.0.0.1:3000/team/workspace/pulls/4",
            "branch": "fix/readme-typo",
            "accepted": false,
        })
        .to_string();
        std::fs::write(state.join("calls.jsonl"), format!("{line}\n")).expect("the ledger");
        let files = NativeFiles {
            root: root.clone(),
            home: root.join("home"),
            tmp: root.join("tmp"),
            config: root.join("config"),
            key: root.join("model.key"),
            key_helper: root.join("helper"),
            program: root.join("program"),
            install_root: root.join("install"),
            skills: Vec::new(),
            mcp_state: Some(state),
            mcp_socket: None,
        };
        let mut transcript = Transcript::open(&run_dir).expect("the transcript");
        let mut state = NativeState::default();
        apply_ledger(&files, &mut transcript, &mut state);
        assert!(
            state.refused_write.is_some(),
            "the no-op was called a write"
        );
        assert!(!state.pr_ok, "the no-op was called remote work");
        let facts = crate::run::facts_from_records(&run_dir);
        assert!(facts.existing.iter().any(|pull| pull.number == 4));
        assert!(
            facts.work_lines().is_empty(),
            "an already-open request alone is not this run's work"
        );
        // A verified push of that branch in the same run names the request.
        crate::run::write_push_witness_at(
            &run_dir,
            "fix/readme-typo",
            "refs/remotes/origin/fix/readme-typo",
            Some("team/workspace"),
            Some("main"),
        )
        .expect("the push witness");
        let facts = crate::run::facts_from_records(&run_dir);
        assert_eq!(
            facts.work_lines(),
            vec!["pull request #4 from fix/readme-typo".to_owned()]
        );
        assert_eq!(
            crate::agent_run::decide_ending(RunEnding::Refused, &facts),
            RunEnding::Succeeded
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r8_a_read_only_check_is_no_work_and_keeps_its_own_sentence() {
        // Walk B F2: the run listed and read its issue, wrote nothing, and said
        // `no work — ...`. The instruction's change word must not turn it into a
        // command failure; the fact that it only read makes it no work.
        let state = NativeState {
            forgejo_read_ok: true,
            ..NativeState::default()
        };
        let summary = "no work — I read issue #1 and nothing needed changing.";
        let base = crate::agent_run::ending_for_instruction(
            "Read the open issues and change what they ask. Say if nothing is needed.",
            summary,
            &RunEvidence {
                read: true,
                ..RunEvidence::default()
            },
        );
        assert_eq!(
            decide_native_ending(base, RequestedWork::GenericWrite, &state),
            RunEnding::NoWork
        );
        assert_eq!(
            run_reason(
                RunEnding::NoWork,
                summary,
                &state,
                RequestedWork::GenericWrite
            )
            .as_deref(),
            Some(summary)
        );
    }

    #[test]
    fn r8_a_local_only_change_is_warned_and_names_the_file() {
        // Walk B F1: the workspace comparison found README.md changed, so the run
        // is a warning even when the instruction's prose asks for a change.
        let state = NativeState {
            workspace_wrote: true,
            local_work: crate::workspace::WorkspaceChanges {
                files: vec!["README.md".to_owned()],
                committed: false,
                git_state: false,
            },
            ..NativeState::default()
        };
        let ending =
            decide_native_ending(RunEnding::Succeeded, RequestedWork::GenericWrite, &state);
        assert_eq!(ending, RunEnding::Warned);
        let reason = run_reason(
            ending,
            "I edited README.md locally.",
            &state,
            RequestedWork::GenericWrite,
        )
        .expect("reason");
        assert!(reason.contains("README.md"), "{reason}");
        assert!(reason.contains("workspace changed"), "{reason}");
    }

    #[test]
    fn r8_a_ledger_open_result_names_the_pull_request_and_its_branch() {
        // The MCP ledger is the supervisor's only record of a native Forgejo write.
        // A real open has no number in its arguments; the reply's number, URL and
        // branch are what make the work line say `pull request #6 from ...`.
        let root = crate::test_support::scratch_root("ledger-pr");
        let run_dir = root.join("run-1700000000005-0001");
        std::fs::create_dir_all(&run_dir).expect("the run directory");
        let state = root.join("mcp-state");
        std::fs::create_dir_all(&state).expect("the MCP state");
        let line = serde_json::json!({
            "tool": crate::forgejo_tools::OPEN_PULL_REQUEST,
            "repository": "team/workspace",
            "number": 6,
            "ok": true,
            "outcome": "opened team/workspace#6",
            "action": "opened",
            "url": "http://127.0.0.1:3000/team/workspace/pulls/6",
            "branch": "agent-1/pr-only",
            "accepted": false,
        })
        .to_string();
        std::fs::write(state.join("calls.jsonl"), format!("{line}\n")).expect("the ledger");
        let files = NativeFiles {
            root: root.clone(),
            home: root.join("home"),
            tmp: root.join("tmp"),
            config: root.join("config"),
            key: root.join("model.key"),
            key_helper: root.join("helper"),
            program: root.join("program"),
            install_root: root.join("install"),
            skills: Vec::new(),
            mcp_state: Some(state),
            mcp_socket: None,
        };
        let mut transcript = Transcript::open(&run_dir).expect("the transcript");
        let mut state = NativeState::default();
        apply_ledger(&files, &mut transcript, &mut state);
        assert!(state.pr_ok);
        let facts = crate::run::facts_from_records(&run_dir);
        assert_eq!(
            facts.work_lines(),
            vec!["pull request #6 from agent-1/pr-only".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn r7_native_redaction_covers_json_escaped_and_split_secrets() {
        let secret = Secret::new("sk-runtime-secret-value".to_owned());
        let escaped = serde_json::to_string(secret.expose()).expect("quoted");
        let escaped = escaped.trim_matches('"');
        let mut carry = String::new();
        let first = redact_native_line(&secret, "prefix sk-runtime-", &mut carry);
        let second = redact_native_line(&secret, "secret-value suffix", &mut carry);
        assert!(first.is_empty(), "the first split line persisted: {first}");
        let text = format!("{first}{second}");
        assert!(!text.contains(secret.expose()), "{text}");
        let line = redact_native_line(&secret, &format!("value={escaped}"), &mut carry);
        assert!(!line.contains(escaped), "{line}");
    }

    #[test]
    fn r2_read_only_comment_succeeds_from_a_read_fact_and_generic_write_needs_a_witness() {
        assert_eq!(
            requested_work("Read the comment on the issue and summarize it.", &[]),
            RequestedWork::ForgejoRead
        );
        let read = NativeState {
            tool_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::ReadOnly, &read),
            RunEnding::NoWork,
            "a read-only run is no work even when the instruction asked for a change"
        );
        assert_eq!(
            requested_work("Rename the parser module.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::GenericWrite, &read),
            RunEnding::NoWork,
            "a successful read is not a workspace write"
        );
    }
    #[test]
    fn c2_issue_update_prefix_and_local_repository_are_not_implicit_writes() {
        assert_eq!(
            requested_work("Read the updated issue and summarize it.", &[]),
            RequestedWork::ForgejoRead
        );
        assert_eq!(
            requested_work("Read the prefix of the issue and report it.", &[]),
            RequestedWork::ForgejoRead
        );
        assert_eq!(
            requested_work("Read the issue and fix the parser.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            requested_work("Read the local repository README and summarize it.", &[]),
            RequestedWork::ReadOnly
        );
    }

    #[test]
    fn c2_supported_and_unsupported_actions_need_matching_facts() {
        let read = NativeState {
            tool_ok: true,
            forgejo_read_ok: true,
            ..NativeState::default()
        };
        assert_eq!(
            requested_work("Close the issue after checking it.", &[]),
            RequestedWork::UnsupportedForgejoWrite
        );
        assert_eq!(
            decide_native_ending(
                RunEnding::Succeeded,
                RequestedWork::UnsupportedForgejoWrite,
                &read
            ),
            RunEnding::NoWork,
            "a run that only read did no work"
        );
        assert_eq!(
            requested_work("Update the pull request description.", &[]),
            RequestedWork::PullRequestUpdate
        );
        let updated = NativeState {
            pr_ok: true,
            pr_updated: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(
                RunEnding::Succeeded,
                RequestedWork::PullRequestUpdate,
                &updated
            ),
            RunEnding::Succeeded
        );
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::PullRequest, &updated),
            RunEnding::Succeeded,
            "an accepted pull-request write is work"
        );
        let opened = NativeState {
            pr_ok: true,
            pushed: true,
            ..NativeState::default()
        };
        assert_eq!(
            decide_native_ending(RunEnding::Succeeded, RequestedWork::PullRequest, &opened),
            RunEnding::Succeeded
        );
    }

    #[test]
    fn c4_multibyte_secret_split_does_not_panic_and_is_redacted() {
        let secret = Secret::new("clé-runtime-é".to_owned());
        let mut carry = String::new();
        let first = redact_native_line(&secret, "prefix clé-runt", &mut carry);
        let second = redact_native_line(&secret, "ime-é suffix", &mut carry);
        assert!(first.is_empty());
        let combined = format!("{first}{second}");
        assert!(!combined.contains(secret.expose()));
    }
    #[test]
    fn c2_nominal_update_and_close_words_are_reads_not_writes() {
        assert_eq!(
            requested_work("Read the update.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the close event.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the PR.", &[]),
            RequestedWork::ForgejoRead
        );
        assert_eq!(
            requested_work("Read the comment.", &[]),
            RequestedWork::ForgejoRead
        );
        assert_eq!(
            requested_work("Read the code comment in src/main.rs.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the comment in src/main.rs.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the local repository README.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the issue and fix the parser.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            requested_work("Review and change the form.", &[]),
            RequestedWork::GenericWrite
        );
    }
    #[test]
    fn c2_action_must_follow_the_connector_and_pr_object_keeps_its_fact() {
        assert_eq!(
            requested_work("Read the update and the logs.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the logs and update the file.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            requested_work("Read the PR in the repository.", &[]),
            RequestedWork::ForgejoRead
        );
        assert_eq!(
            requested_work("Read the code comment in src/main.rs.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the local repository README.", &[]),
            RequestedWork::ReadOnly
        );
        assert_eq!(
            requested_work("Read the issue and fix the parser.", &[]),
            RequestedWork::GenericWrite
        );
        assert_eq!(
            requested_work("Review and change the form.", &[]),
            RequestedWork::GenericWrite
        );
    }
    #[test]
    fn a1_codex_config_approves_exactly_the_six_product_tools() {
        let text = codex_tool_approvals();
        assert_eq!(
            text.matches("[mcp_servers.gewu_forgejo.tools.").count(),
            crate::forgejo_tools::TOOL_NAMES.len()
        );
        assert_eq!(text.matches("approval_mode = \"approve\"").count(), 6);
        for tool in crate::forgejo_tools::TOOL_NAMES {
            assert!(text.contains(&format!(".tools.{tool}]")), "missing {tool}");
        }
        for forbidden in [
            "default_tools_approval_mode",
            "approval_policy",
            "enabled_tools",
            "mcp_servers.gewu_forgejo.tools.default",
        ] {
            assert!(!text.contains(forbidden), "unexpected {forbidden}");
        }
    }

    #[test]
    fn a2_non_success_reason_never_repeats_success_prose() {
        let prose = "I read the issue, pushed, opened the pull request, and commented.";
        let refused = NativeState {
            refused_write: Some(
                "MCP tool call requires approval, but approval policy is never".to_owned(),
            ),
            ..NativeState::default()
        };
        assert_eq!(
            run_reason(
                RunEnding::Refused,
                prose,
                &refused,
                RequestedWork::Ambiguous
            )
            .as_deref(),
            Some("MCP tool call requires approval, but approval policy is never")
        );
        let first = NativeState {
            first_failure: Some("shell: the native sandbox boundary refused this command (bubblewrap: Permission denied)".to_owned()),
            refused_write: Some("forgejo_update_pull_request: no open pull request".to_owned()),
            ..NativeState::default()
        };
        let first_reason = run_reason(RunEnding::Refused, prose, &first, RequestedWork::Ambiguous)
            .expect("reason");
        assert!(first_reason.contains("bubblewrap"), "{first_reason}");
        assert!(
            !first_reason.contains("no open pull request"),
            "{first_reason}"
        );
        let uncertain = NativeState {
            uncertain_write: true,
            ..NativeState::default()
        };
        let reason = run_reason(
            RunEnding::Warned,
            prose,
            &uncertain,
            RequestedWork::Ambiguous,
        )
        .expect("reason");
        assert!(!reason.contains("I read the issue"));
        assert!(reason.contains("uncertain"));
        assert_eq!(
            run_reason(
                RunEnding::Succeeded,
                prose,
                &refused,
                RequestedWork::Ambiguous
            )
            .as_deref(),
            Some(prose)
        );
        // No work is the run's own neutral sentence, not a generic failure. It is
        // the reason the card shows after `∅`, so it must be kept.
        assert_eq!(
            run_reason(
                RunEnding::NoWork,
                "nothing new to add",
                &NativeState::default(),
                RequestedWork::Ambiguous
            )
            .as_deref(),
            Some("nothing new to add")
        );
        // A warning that is not a refusal never repeats the success prose.
        assert!(
            !run_reason(
                RunEnding::Warned,
                prose,
                &NativeState::default(),
                RequestedWork::Ambiguous
            )
            .expect("reason")
            .contains("I read the issue")
        );
    }
}

/// Replay the failure-signal inventory's probe streams through the same parser the
/// live supervisor uses. The fixture is generated from the inventory by
/// `tests/fixtures/generate_outcome_corpus.py`, which replaces private roots and
/// session ids; each entry asserts the inventory's correct terminal outcome and the
/// truthful part of its reason.
#[cfg(test)]
mod corpus_tests {
    use super::*;

    fn disposition_label(disposition: Disposition) -> &'static str {
        match disposition {
            Disposition::Succeeded => "succeeded",
            Disposition::Failed => "failed",
            Disposition::Stopped => "stopped",
            Disposition::TimedOut => "timed_out",
        }
    }

    #[test]
    fn every_inventory_probe_replays_through_the_native_outcome_parser() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native-outcome-corpus.json"
        ))
        .expect("the inventory corpus fixture is JSON");
        let entries = corpus["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 88, "the inventory has 88 probes");
        assert_eq!(
            corpus["inventory_sha256"].as_str(),
            Some("f5460c0a5794b61a58f7ffddb49f50f439685f8e0e7691af8e1ba895c5b57d39"),
            "the corpus records the inventory it was generated from"
        );
        let mut failures = Vec::new();
        for entry in entries {
            let id = entry["probe_id"].as_str().unwrap_or("?");
            let program = Program::parse(entry["harness"].as_str().unwrap_or_default())
                .unwrap_or_else(|| panic!("{id}: known harness"));
            let mut outcome = NativeOutcome::new(program);
            for line in entry["stdout"].as_array().expect("stdout lines") {
                outcome.observe_line(line.as_str().unwrap_or_default());
            }
            let exit = entry["exit_code"].as_i64().map(|code| code as i32);
            let expected = &entry["expected"];
            let supervisor = match expected["supervisor"].as_str().unwrap_or("natural") {
                "stopped" => SupervisorEnd::Stopped,
                "deadline" => SupervisorEnd::Deadline,
                _ => SupervisorEnd::Natural,
            };
            let resolution = outcome.finish(exit, supervisor);
            let stderr: Vec<String> = entry["stderr"]
                .as_array()
                .expect("stderr lines")
                .iter()
                .map(|line| line.as_str().unwrap_or_default().to_owned())
                .collect();
            let mut reason = resolution.reason.clone();
            if reason.as_deref().is_none_or(|reason| {
                reason.starts_with("the native program exited with code")
                    || reason == "the native program was ended by a signal"
            }) {
                reason = stderr_cause(&stderr).or(reason);
            }
            let reason_lower = reason.clone().unwrap_or_default().to_ascii_lowercase();

            let want = expected["disposition"].as_str().unwrap_or_default();
            let got = disposition_label(resolution.disposition);
            if got != want {
                failures.push(format!(
                    "{id}: disposition {got}, inventory says {want} (reason {reason:?})"
                ));
                continue;
            }
            for fragment in expected["reason_contains"]
                .as_array()
                .expect("reason fragments")
            {
                let fragment = fragment.as_str().unwrap_or_default();
                if !reason_lower.contains(&fragment.to_ascii_lowercase()) {
                    failures.push(format!(
                        "{id}: reason {reason:?} does not contain {fragment:?}"
                    ));
                }
            }
            if let Some(text) = expected["text"].as_str()
                && resolution.text.as_deref() != Some(text)
            {
                failures.push(format!(
                    "{id}: committed text {:?}, expected {text:?}",
                    resolution.text
                ));
            }
            if expected["text_absent"].as_bool() == Some(true) && resolution.text.is_some() {
                failures.push(format!(
                    "{id}: text from a failed attempt was committed: {:?}",
                    resolution.text
                ));
            }
            if let Some(saw_tool) = expected["saw_tool"].as_bool()
                && resolution.saw_tool != saw_tool
            {
                failures.push(format!(
                    "{id}: saw_tool {}, expected {saw_tool}",
                    resolution.saw_tool
                ));
            }
            if let Some(fragments) = expected["tool_failure_contains"].as_array() {
                let failure = resolution
                    .first_tool_failure
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                for fragment in fragments {
                    let fragment = fragment.as_str().unwrap_or_default();
                    if !failure.contains(&fragment.to_ascii_lowercase()) {
                        failures.push(format!(
                            "{id}: tool failure {:?} does not contain {fragment:?}",
                            resolution.first_tool_failure
                        ));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "the inventory corpus found {} mismatch(es):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

#[cfg(test)]
mod unconfirmed_work_tests {
    use super::*;

    #[test]
    fn work_that_needs_a_tool_is_never_success_without_a_confirmed_tool_result() {
        let state = NativeState::new(Program::Codex);
        let reason = non_success_reason(&state, RequestedWork::GenericWrite);
        assert!(reason.contains("no tool call"), "{reason}");
        assert!(reason.contains("sandbox can refuse"), "{reason}");
        assert!(reason.contains("unconfirmed"), "{reason}");
        assert!(!requires_confirmed_tool(RequestedWork::Ambiguous));
        assert!(requires_confirmed_tool(RequestedWork::ReadOnly));
    }

    #[test]
    fn the_stderr_cause_is_the_first_error_sentence_not_the_last_stack_line() {
        let codex = vec![
            "Error loading config.toml:".to_owned(),
            "/home/user/config.toml:1:1: invalid key-value pair, expected key".to_owned(),
            "  |".to_owned(),
            "1 | { this is not valid config [".to_owned(),
            "  | ^".to_owned(),
        ];
        let reason = stderr_cause(&codex).expect("the codex config cause");
        assert!(reason.contains("config.toml"), "{reason}");
        assert!(reason.contains("invalid key-value pair"), "{reason}");
        assert!(!reason.contains('^'), "{reason}");

        let dsh = vec![
            "file:///dsh/lib/index.js:3998".to_owned(),
            "    at parsePatchList".to_owned(),
            "Error: dsh: failed to parse overlay /tmp/overlay.yml: YAMLException".to_owned(),
            "    at loadOverlayPatches".to_owned(),
            "Node.js v24.19.0".to_owned(),
        ];
        let reason = stderr_cause(&dsh).expect("the dsh overlay cause");
        assert!(reason.contains("failed to parse overlay"), "{reason}");
        assert!(!reason.contains("Node.js"), "{reason}");
    }
}

#[cfg(test)]
mod launch_budget_tests {
    use super::*;

    #[test]
    fn the_harness_retry_budget_fits_inside_the_session_deadline() {
        for timeout_secs in [5_u64, 30, 60, 900] {
            let budget = launch_budget(timeout_secs);
            let worst = budget.request_ms * 3 + budget.retry_delay_ms + budget.retry_delay_ms * 2;
            assert!(
                worst < timeout_secs * 1000,
                "a {timeout_secs} s session can spend {worst} ms retrying"
            );
            assert!(budget.request_ms >= 1_000);
            assert!(budget.request_ms <= 120_000);
            if timeout_secs >= 60 {
                assert!(opencode_request_ms(timeout_secs) * 12 < timeout_secs * 1000);
            }
        }
    }
}

/// Correction-1 cases at the shared parser: text and causes belong to the
/// attempt/tool that produced them. Failed-attempt text is never committed, and
/// a failed tool is a deadline warning, not a session failure.
#[cfg(test)]
mod attempt_replay_tests {
    use super::*;

    #[test]
    fn attempt_ownership_cases_replay_through_the_same_parser() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native-outcome-attempt-cases.json"
        ))
        .expect("the attempt corpus fixture is JSON");
        let entries = corpus["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 9);
        let mut failures = Vec::new();
        for entry in entries {
            let id = entry["case"].as_str().unwrap_or("?");
            let program = Program::parse(entry["harness"].as_str().unwrap_or_default())
                .unwrap_or_else(|| panic!("{id}: known harness"));
            let mut outcome = NativeOutcome::new(program);
            for line in entry["stdout"].as_array().expect("stdout") {
                outcome.observe_line(line.as_str().unwrap_or_default());
            }
            let exit = entry["exit_code"].as_i64().map(|code| code as i32);
            let supervisor = match entry["supervisor"].as_str().unwrap_or("natural") {
                "deadline" => SupervisorEnd::Deadline,
                "stopped" => SupervisorEnd::Stopped,
                _ => SupervisorEnd::Natural,
            };
            let expected = &entry["expected"];
            let resolution = outcome.finish(exit, supervisor);
            let want = expected["disposition"].as_str().unwrap_or_default();
            let got = match resolution.disposition {
                Disposition::Succeeded => "succeeded",
                Disposition::Failed => "failed",
                Disposition::Stopped => "stopped",
                Disposition::TimedOut => "timed_out",
            };
            if got != want {
                failures.push(format!(
                    "{id}: disposition {got}, expected {want} (reason {:?})",
                    resolution.reason
                ));
            }
            if expected["text_absent"].as_bool() == Some(true) && resolution.text.is_some() {
                failures.push(format!(
                    "{id}: failed-attempt text was committed: {:?}",
                    resolution.text
                ));
            }
            let reason = resolution
                .reason
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            for fragment in expected["reason_contains"].as_array().into_iter().flatten() {
                let fragment = fragment.as_str().unwrap_or_default().to_ascii_lowercase();
                if !reason.contains(&fragment) {
                    failures.push(format!(
                        "{id}: reason {:?} lacks {fragment:?}",
                        resolution.reason
                    ));
                }
            }
            if let Some(fragments) = expected["tool_failure_contains"].as_array() {
                let failure = resolution
                    .first_tool_failure
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                for fragment in fragments {
                    let fragment = fragment.as_str().unwrap_or_default().to_ascii_lowercase();
                    if !failure.contains(&fragment) {
                        failures.push(format!(
                            "{id}: tool warning {:?} lacks {fragment:?}",
                            resolution.first_tool_failure
                        ));
                    }
                }
            }
            if supervisor == SupervisorEnd::Deadline {
                // The deadline cause is provider/terminal only. A failed tool is
                // persisted as a warning so the engine can name it without
                // changing the TimedOut ending.
                if outcome.deadline_cause().is_some() {
                    failures.push(format!("{id}: a failed tool became the deadline cause"));
                }
                let warning = outcome
                    .progress()
                    .and_then(|(_, _, _, warning)| warning)
                    .unwrap_or_default();
                if !warning.to_ascii_lowercase().contains("exit code 1") {
                    failures.push(format!(
                        "{id}: the deadline warning is not the tool result: {warning:?}"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "attempt ownership replay found {} mismatch(es):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
