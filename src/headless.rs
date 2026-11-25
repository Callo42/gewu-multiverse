//! Non-interactive configuration (`--configure`), unattended daemon execution
//! (`--headless`), live runtime agent control (`--control`), and lock-free read-only
//! inspection (`--status`).
//!
//! All modes share the engine's layout, `0600` credentials boundary, Landlock
//! detection and single-instance lock rules without requiring a terminal. Secrets
//! never enter command-line arguments, control socket messages, status output,
//! snapshots or logs.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use signal_hook::SigId;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::bundled_forgejo::{self, InstancePaths};
use crate::cli::{ConfigureCli, ControlCli, HeadlessCli, StatusCli};
use crate::confinement;
use crate::credentials;
use crate::engine::settings::{self, Settings};
use crate::engine::{self, Engine, EngineOptions, TERMS_VERSION};
use crate::engine_api::{
    BoardView, Command, Desired, EndpointTest, EngineApi, ForgejoState, Provisioning, SessionEnd,
    SessionState,
};
use crate::harness::{self, Program, Protocol};
use crate::mode::{PRIVATE_DIR_MODE, SECRET_FILE_MODE, set_mode};
use crate::paths::Layout;
use crate::run::{self, process_alive};
use crate::time::now_unix_ms;

/// Private runtime heartbeat/snapshot written by `--headless` under the state root.
pub const STATUS_SNAPSHOT_FILE: &str = "headless-status.json";

/// Default private Unix control socket bound by `--headless` under the state root.
pub const CONTROL_SOCKET_FILE: &str = "headless.sock";

/// Upper bound on a single `--control` request payload.
const MAX_CONTROL_REQUEST_BYTES: u64 = 16 * 1024;

/// Upper bound on a single `--control` response payload.
const MAX_CONTROL_RESPONSE_BYTES: u64 = 512 * 1024;

/// Server-side socket read/write bound so a stalled client cannot block `run_headless`.
const SERVER_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

/// Client-side socket read/write bound (covers polite `stop_grace` when stopping a run).
const CLIENT_SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

/// Resolved layout and data directory for one invocation.
pub fn resolve_paths(
    root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    data_root: Option<PathBuf>,
) -> Result<(Layout, PathBuf), String> {
    let layout = match root {
        Some(root) => Layout::rooted_at(root).map_err(|error| error.to_string())?,
        None => Layout::in_user_config_dir().map_err(|error| error.to_string())?,
    };
    let layout = match state_root {
        Some(state) => layout
            .with_state_root(state)
            .map_err(|error| error.to_string())?,
        None => layout,
    };
    let data = match data_root {
        Some(dir) => absolute_dir(&dir)?,
        None => bundled_forgejo::data_root()?,
    };
    Ok((layout, data))
}

fn absolute_dir(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|error| format!("{} cannot be made absolute: {error}", path.display()))?;
    Ok(cwd.join(path))
}

/// Non-blocking check whether another engine currently holds `engine.lock`.
pub fn engine_lock_held(data_root: &Path) -> bool {
    let path = bundled_forgejo::lock_path(data_root);
    if !path.is_file() {
        return false;
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            drop(lock);
            false
        }
        Err((_, error)) => {
            error == nix::errno::Errno::EWOULDBLOCK || error == nix::errno::Errno::EAGAIN
        }
    }
}

fn read_secret_input(
    key_file: Option<&Path>,
    key_stdin: bool,
) -> Result<Option<String>, String> {
    if let Some(path) = key_file {
        let raw = fs::read_to_string(path)
            .map_err(|error| format!("API key file {} cannot be read: {error}", path.display()))?;
        let trimmed = raw.trim().to_owned();
        if trimmed.is_empty() {
            return Err(format!("API key file {} is empty", path.display()));
        }
        return Ok(Some(trimmed));
    }
    if key_stdin {
        let mut raw = String::new();
        io::stdin()
            .read_to_string(&mut raw)
            .map_err(|error| format!("API key could not be read from stdin: {error}"))?;
        let trimmed = raw.trim().to_owned();
        if trimmed.is_empty() {
            return Err("stdin carried an empty API key".to_owned());
        }
        return Ok(Some(trimmed));
    }
    Ok(None)
}

/// Execute `gewu-multiverse --configure ...` without a terminal or starting Forgejo.
pub fn run_configure(cli: ConfigureCli) -> Result<Vec<String>, String> {
    let key = read_secret_input(cli.api_key_file.as_deref(), cli.api_key_stdin)?;
    let (layout, data_root) = resolve_paths(
        cli.root.clone(),
        cli.state_root.clone(),
        cli.data_root.clone(),
    )?;
    if engine_lock_held(&data_root) {
        return Err(format!(
            "another gewu-multiverse instance is running (using {}); stop it before running --configure",
            data_root.display()
        ));
    }
    let mut options = EngineOptions::new(layout.clone(), &data_root)?;
    options.auto_start_forgejo = false;
    let stop_grace = options.stop_grace;
    let mut engine = engine::open_with(options)?;
    let result = apply_configure(&mut engine, &cli, key, &layout, &data_root);
    engine.shutdown(stop_grace);
    result
}

fn apply_configure(
    engine: &mut engine::Engine,
    cli: &ConfigureCli,
    key: Option<String>,
    layout: &Layout,
    data_root: &Path,
) -> Result<Vec<String>, String> {
    let mut summary: Vec<String> = Vec::new();

    let requested_protocol = match &cli.protocol {
        Some(text) => Some(
            Protocol::parse(text).ok_or_else(|| {
                format!(
                    "unknown protocol `{text}` (choose chat-completions, responses or anthropic-messages)"
                )
            })?,
        ),
        None => None,
    };

    let requested_program = match &cli.program {
        Some(program_raw) => Some(Program::parse(program_raw).ok_or_else(|| {
            format!(
                "unknown program `{program_raw}` (choose builtin, pi, codex, dsh or opencode)"
            )
        })?),
        None => None,
    };

    let requested_codex_sandbox = match &cli.codex_sandbox {
        Some(mode) => {
            let trimmed = mode.trim();
            if trimmed != "native" && trimmed != "full-access" {
                return Err(format!(
                    "--codex-sandbox must be `native` or `full-access` (got `{trimmed}`)"
                ));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };

    if cli.accept_terms {
        let view = engine.board();
        for (idx, line) in view.terms.iter().enumerate() {
            let _ = writeln!(io::stderr(), "gewu-multiverse: term {}: {line}", idx + 1);
        }
        engine.send(Command::AcknowledgeTerms)?;
        summary.push(format!("acknowledged terms (version {TERMS_VERSION})"));
    }

    let has_conn_inputs =
        cli.base_url.is_some() || cli.protocol.is_some() || cli.model.is_some() || key.is_some();
    let defer_conn_to_agents = cli.agent.is_some()
        || requested_program.is_some_and(Program::is_native)
        || requested_protocol.is_some_and(|protocol| protocol != Protocol::ChatCompletions);

    if has_conn_inputs && !defer_conn_to_agents {
        let current = engine.board().endpoint;
        let base_url = cli
            .base_url
            .clone()
            .unwrap_or_else(|| current.base_url.clone());
        let model = cli.model.clone().unwrap_or_else(|| current.model.clone());
        let protocol = requested_protocol.unwrap_or(Protocol::ChatCompletions);
        if base_url.trim().is_empty() || model.trim().is_empty() {
            return Err(
                "setting the default connection requires both --base-url <url> and --model <id>"
                    .to_owned(),
            );
        }
        engine.send(Command::SetConnection {
            base_url: base_url.clone(),
            protocol: protocol.id().to_owned(),
            model: model.clone(),
            key: key.clone(),
        })?;
        summary.push(format!(
            "configured default connection ({}, model `{model}`)",
            protocol.id()
        ));
    }

    if let Some(count) = cli.agents {
        engine.send(Command::CreateAgents { count })?;
        summary.push(format!("ensured {count} agent(s)"));
    }
    if cli.add_agent {
        engine.send(Command::AddAgent)?;
        if let Some(last) = engine.board().agents.last() {
            summary.push(format!("added agent `{}`", last.name));
        }
    }
    if let Some(remove) = &cli.remove_agent {
        if !engine.board().agents.iter().any(|a| &a.name == remove) {
            return Err(format!("there is no agent named `{remove}`"));
        }
        let creds_path = layout.credentials_file();
        let has_forgejo_state = InstancePaths::under(data_root).app_ini.is_file()
            || credentials::recorded_login(&creds_path, remove)
                .ok()
                .flatten()
                .is_some()
            || credentials::forgejo_agent_token_optional(&creds_path, remove)
                .ok()
                .flatten()
                .is_some()
            || engine::removal_requested(layout, remove);
        if !has_forgejo_state {
            engine.remove_unprovisioned_agent(remove)?;
            summary.push(format!("removed agent `{remove}`"));
        } else {
            engine.send(Command::Stop {
                name: remove.clone(),
            })?;
            engine.mark_removal_requested(remove)?;
            engine.send(Command::StartForgejo)?;
            wait_for_agent_removal(engine, remove)?;
            summary.push(format!(
                "removed agent `{remove}` and revoked its Forgejo sign-in"
            ));
        }
    }

    let need_agent_targets = requested_program.is_some()
        || requested_codex_sandbox.is_some()
        || cli.instruction_file.is_some()
        || (has_conn_inputs && defer_conn_to_agents);

    let target_agents: Vec<String> = if need_agent_targets {
        let existing: Vec<String> = engine
            .board()
            .agents
            .into_iter()
            .map(|agent| agent.name)
            .collect();
        if let Some(explicit) = &cli.agent {
            if !existing.iter().any(|name| name == explicit) {
                return Err(format!("there is no agent named `{explicit}`"));
            }
            vec![explicit.clone()]
        } else if existing.is_empty() {
            return Err(
                "no agents exist yet; pass `--agents <count>` (or `--add-agent`) before configuring an agent"
                    .to_owned(),
            );
        } else if existing.len() == 1 {
            existing
        } else if cli.instruction_file.is_some() {
            return Err(
                "multiple agents exist; pass `--agent <name>` with `--instruction-file`"
                    .to_owned(),
            );
        } else if cli.agents.is_some() {
            existing
        } else {
            return Err(
                "multiple agents exist; pass `--agent <name>` to select which agent to configure"
                    .to_owned(),
            );
        }
    } else {
        Vec::new()
    };

    for name in &target_agents {
        if engine::removal_requested(layout, name) {
            return Err(format!(
                "agent `{name}` is queued for removal; finish removing it with `gewu-multiverse --configure --remove-agent {name}` before changing its configuration"
            ));
        }
        let pre_board = engine.board();
        let pre_view = pre_board
            .agents
            .iter()
            .find(|a| &a.name == name)
            .ok_or_else(|| format!("there is no agent named `{name}`"))?;
        let effective_program = requested_program
            .unwrap_or_else(|| Program::parse(&pre_view.program).unwrap_or(Program::Builtin));
        let chosen_protocol = if let Some(req) = requested_protocol {
            if let Some(err) = harness::compatibility_error(effective_program, req) {
                return Err(err);
            }
            req
        } else if effective_program == Program::Builtin {
            Protocol::ChatCompletions
        } else {
            Protocol::parse(&pre_view.protocol)
                .filter(|p| effective_program.supports(*p))
                .unwrap_or_else(|| effective_program.supported_protocols()[0])
        };

        let conn_inputs_ready = if has_conn_inputs && defer_conn_to_agents {
            let base_url = cli
                .base_url
                .clone()
                .unwrap_or_else(|| pre_view.base_url.clone());
            let model = cli.model.clone().unwrap_or_else(|| pre_view.model.clone());
            if base_url.trim().is_empty() || model.trim().is_empty() {
                return Err(format!(
                    "configuring model for `{name}` requires both --base-url <url> and --model <id>"
                ));
            }
            Some((base_url, model))
        } else {
            None
        };

        if let Some(program) = requested_program {
            let proto_for_program = if program.is_native() && has_conn_inputs {
                Some(chosen_protocol.id().to_owned())
            } else {
                requested_protocol.map(|p| p.id().to_owned())
            };
            engine.send(Command::SetAgentProgram {
                name: name.clone(),
                program: program.id().to_owned(),
                protocol: proto_for_program,
            })?;
            summary.push(format!("agent `{name}` program set to `{}`", program.id()));
        }

        if let Some(mode) = &requested_codex_sandbox {
            engine.send(Command::SetAgentCodexSandbox {
                name: name.clone(),
                mode: mode.clone(),
            })?;
            summary.push(format!("agent `{name}` codex sandbox set to `{mode}`"));
        }

        if let Some((base_url, model)) = conn_inputs_ready {
            let protocol = chosen_protocol.id().to_owned();
            if effective_program == Program::Builtin {
                // Built-in agents execute against the shared `settings.endpoint`.
                engine.send(Command::SetConnection {
                    base_url,
                    protocol: protocol.clone(),
                    model: model.clone(),
                    key: key.clone(),
                })?;
                summary.push(format!(
                    "configured default connection for built-in `{name}` ({protocol}, model `{model}`)"
                ));
            } else {
                engine.send(Command::SetAgentModel {
                    name: name.clone(),
                    base_url,
                    protocol: protocol.clone(),
                    model: model.clone(),
                    key: key.clone(),
                })?;
                summary.push(format!(
                    "configured model for `{name}` ({protocol}, model `{model}`)"
                ));
            }
        }

        if let Some(inst_file) = &cli.instruction_file {
            let text = fs::read_to_string(inst_file).map_err(|error| {
                format!(
                    "instruction file {} cannot be read: {error}",
                    inst_file.display()
                )
            })?;
            if text.trim().is_empty() {
                return Err(format!(
                    "instruction file {} is empty; write a non-empty instruction",
                    inst_file.display()
                ));
            }
            let target_path = engine
                .instruction_path(name)
                .ok_or_else(|| format!("cannot resolve instruction path for `{name}`"))?;
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("{} cannot be created: {error}", parent.display())
                })?;
            }
            crate::mode::write_private(&target_path, text.as_bytes()).map_err(|error| {
                format!("{} cannot be written: {error}", target_path.display())
            })?;
            engine.refresh_instructions();
            summary.push(format!(
                "wrote instruction for `{name}` ({} chars)",
                text.chars().count()
            ));
        }
    }

    let mut to_stop = cli.stop_agents.clone();
    if cli.stop_all {
        let all: Vec<String> = engine
            .board()
            .agents
            .into_iter()
            .map(|a| a.name)
            .collect();
        if all.is_empty() {
            return Err("cannot `--stop-all`: no agents are configured".to_owned());
        }
        for name in all {
            if !to_stop.contains(&name) {
                to_stop.push(name);
            }
        }
    }

    for stop_name in &to_stop {
        if engine::removal_requested(layout, stop_name) {
            if cli.stop_all {
                continue;
            }
            return Err(format!(
                "agent `{stop_name}` is already stopped and queued for removal; finish removing it with `gewu-multiverse --configure --remove-agent {stop_name}`"
            ));
        }
        engine.send(Command::Stop {
            name: stop_name.clone(),
        })?;
        summary.push(format!("stopped agent `{stop_name}`"));
    }

    let mut to_start = cli.start_agents.clone();
    if cli.start_all {
        let all: Vec<String> = engine
            .board()
            .agents
            .into_iter()
            .map(|a| a.name)
            .collect();
        if all.is_empty() {
            return Err("cannot `--start-all`: no agents are configured".to_owned());
        }
        for name in all {
            if !to_start.contains(&name) {
                to_start.push(name);
            }
        }
    }

    if !to_start.is_empty() {
        engine.refresh_instructions();
        let pre_start = engine.board();
        for start_name in &to_start {
            let agent = pre_start
                .agents
                .iter()
                .find(|a| &a.name == start_name)
                .ok_or_else(|| format!("there is no agent named `{start_name}`"))?;
            if engine::removal_requested(layout, start_name) {
                return Err(format!(
                    "agent `{start_name}` is queued for removal; finish `gewu-multiverse --configure --remove-agent {start_name}` first"
                ));
            }
            if let Some(err) = &agent.program_error {
                return Err(format!(
                    "agent `{start_name}` cannot start: {err} — {}",
                    repair_command_for_agent_block(start_name, &agent.program, err)
                ));
            }
            if agent.instruction_chars == 0 {
                return Err(format!(
                    "agent `{start_name}` cannot start: instruction is empty — run `gewu-multiverse --configure --agent {start_name} --instruction-file <file>`"
                ));
            }
        }
        for start_name in &to_start {
            engine.send(Command::Start {
                name: start_name.clone(),
            })?;
        }
        let board = engine.board();
        for start_name in &to_start {
            let agent = board
                .agents
                .iter()
                .find(|a| &a.name == start_name)
                .ok_or_else(|| format!("there is no agent named `{start_name}`"))?;
            if let Some(err) = &agent.program_error {
                return Err(format!(
                    "agent `{start_name}` cannot start: {err} — {}",
                    repair_command_for_agent_block(start_name, &agent.program, err)
                ));
            }
            if agent.instruction_chars == 0 {
                return Err(format!(
                    "agent `{start_name}` cannot start: instruction is empty — run `gewu-multiverse --configure --agent {start_name} --instruction-file <file>`"
                ));
            }
            if let SessionState::Blocked { reason } = &agent.session
                && reason != "waiting for Forgejo"
                && reason != "account not ready"
            {
                return Err(format!("agent `{start_name}` cannot start: {reason}"));
            }
            if agent.desired != Desired::Started {
                return Err(format!(
                    "agent `{start_name}` did not enter the started state"
                ));
            }
            summary.push(format!("marked agent `{start_name}` started"));
        }
    }

    if cli.test_model {
        let board = engine.board();
        let selected_agent = if let Some(explicit) = &cli.agent {
            let found = board
                .agents
                .iter()
                .find(|a| &a.name == explicit)
                .ok_or_else(|| format!("there is no agent named `{explicit}`"))?;
            Some((explicit.clone(), found.program != "builtin"))
        } else if target_agents.len() == 1 {
            let name = &target_agents[0];
            board
                .agents
                .iter()
                .find(|a| &a.name == name)
                .map(|a| (name.clone(), a.program != "builtin"))
        } else {
            None
        };
        if let Some((name, true)) = selected_agent {
            engine.send(Command::TestAgentModel { name: name.clone() })?;
            wait_for_model_test(engine, Some(&name))?;
            summary.push(format!("model connection test passed for `{name}`"));
        } else {
            engine.send(Command::TestEndpoint)?;
            wait_for_model_test(engine, None)?;
            summary.push("default model connection test passed".to_owned());
        }
    }

    Ok(summary)
}

/// Drive only Forgejo startup/admin-token setup and `RemoveAgent` for `name` (never
/// scheduling sessions or provisioning other agents). Note: per `credentials::remove_agent_token`,
/// secret entries (`forgejo_agent_tokens`, `forgejo_token_names`, `model_keys`) are removed while
/// non-secret identity history (`forgejo_logins`, `forgejo_emails`, `forgejo_identity_*`) is
/// intentionally kept so account numbers and logins are never recycled.
fn wait_for_agent_removal(engine: &mut engine::Engine, name: &str) -> Result<(), String> {
    let deadline = Instant::now() + engine::DEFAULT_READY_TIMEOUT;
    loop {
        engine.tick_removal_only(name);
        let board = engine.board();
        if !board.agents.iter().any(|a| a.name == name) {
            return Ok(());
        }
        if let ForgejoState::Failed { reason } = &board.forgejo.state {
            return Err(format!(
                "agent `{name}` was stopped and queued for removal, but Forgejo could not start to revoke its sign-in ({reason}) — repair Forgejo with `gewu-multiverse --install-forgejo` and rerun `gewu-multiverse --configure --remove-agent {name}`"
            ));
        }
        if let Some(note) = board.notice.as_deref()
            && (note.contains("login could not be cancelled")
                || note.contains("could not be removed")
                || note.contains("removal could not be saved"))
        {
            return Err(format!(
                "agent `{name}` was stopped and queued for removal, but removal did not finish ({note}) — rerun `gewu-multiverse --configure --remove-agent {name}`"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "agent `{name}` was stopped and queued for removal, but Forgejo did not finish revoking its sign-in within {}s — rerun `gewu-multiverse --configure --remove-agent {name}`",
                engine::DEFAULT_READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn repair_command_for_agent_block(name: &str, program: &str, err: &str) -> String {
    if err.contains("not installed") {
        format!(
            "install it with `gewu-multiverse --install-harness {program}` (or switch to the built-in engine with `gewu-multiverse --configure --agent {name} --program builtin`)"
        )
    } else if err.contains("model key") {
        format!(
            "set a key with `gewu-multiverse --configure --agent {name} --api-key-file <file>` (or `--api-key-stdin`)"
        )
    } else if err.contains("Responses")
        || err.contains("Chat Completions")
        || err.contains("Anthropic")
    {
        format!(
            "choose a compatible protocol with `gewu-multiverse --configure --agent {name} --protocol <chat-completions|responses|anthropic-messages> --base-url <url> --model <id>`"
        )
    } else if err.contains("model") || err.contains("connection") {
        format!(
            "configure the model with `gewu-multiverse --configure --agent {name} --base-url <url> --model <id> --api-key-file <file>`"
        )
    } else if program == "codex" {
        format!(
            "adjust Codex sandbox setup with `gewu-multiverse --configure --agent {name} --codex-sandbox full-access` (or switch program with `--program builtin`)"
        )
    } else {
        format!(
            "reconfigure `{name}` with `gewu-multiverse --configure --agent {name} --program builtin --base-url <url> --model <id> --api-key-file <file>`"
        )
    }
}

fn wait_for_model_test(engine: &mut engine::Engine, agent: Option<&str>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        engine.tick();
        let board = engine.board();
        let state = match agent {
            Some(name) => board
                .agents
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.model_test.clone())
                .ok_or_else(|| format!("there is no agent named `{name}`"))?,
            None => board.endpoint.test,
        };
        match state {
            EndpointTest::Passed { .. } => return Ok(()),
            EndpointTest::Failed { reason } => {
                return Err(format!("model connection test failed: {reason}"));
            }
            EndpointTest::NotRun | EndpointTest::Running => {
                if Instant::now() >= deadline {
                    return Err("model connection test timed out after 25s".to_owned());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Signal watcher for `SIGINT`, `SIGTERM`, and `SIGHUP` in `--headless` mode.
struct DaemonSignals {
    flag: Arc<AtomicBool>,
    ids: Vec<SigId>,
}

impl DaemonSignals {
    fn install() -> Result<Self, String> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            let id = signal_hook::flag::register(signal, Arc::clone(&flag))
                .map_err(|error| format!("cannot install signal handler: {error}"))?;
            ids.push(id);
        }
        Ok(Self { flag, ids })
    }

    fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

impl Drop for DaemonSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// Private Unix control socket and `0600` capability cookie bound by `--headless` and
/// unlinked on exit. Pairing the socket with a filesystem-read capability cookie inside
/// the hidden parent directory prevents same-UID Landlock-confined agent shells from
/// issuing control requests even though Linux Landlock `AccessFs` does not hook
/// pathname `AF_UNIX` `connect(2)`.
struct ControlSocketGuard {
    listener: UnixListener,
    path: PathBuf,
    dev: u64,
    ino: u64,
    cookie: String,
    cookie_path: PathBuf,
    cookie_dev: u64,
    cookie_ino: u64,
}

pub fn resolve_control_socket_path(
    layout: &Layout,
    custom: Option<&Path>,
) -> Result<PathBuf, String> {
    let Some(raw) = custom else {
        return Ok(layout.state_root().join(CONTROL_SOCKET_FILE));
    };
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("current directory cannot be read: {error}"))?
            .join(raw)
    };
    let Some(name) = abs.file_name() else {
        return Err(format!(
            "--control-socket {} must name a socket file inside a private 0700 directory",
            raw.display()
        ));
    };
    if name == "." || name == ".." {
        return Err(format!(
            "--control-socket {} must name a socket file, not a directory",
            raw.display()
        ));
    }
    Ok(abs)
}

impl ControlSocketGuard {
    fn bind(
        layout: &Layout,
        data_root: &Path,
        custom: Option<&Path>,
        view: &BoardView,
    ) -> Result<Self, String> {
        let requested = resolve_control_socket_path(layout, custom)?;
        if custom.is_none() {
            fs::create_dir_all(layout.state_root()).map_err(|error| {
                format!("{} cannot be created: {error}", layout.state_root().display())
            })?;
            set_mode(layout.state_root(), PRIVATE_DIR_MODE).map_err(|error| {
                format!(
                    "{} permissions cannot be set to 0700: {error}",
                    layout.state_root().display()
                )
            })?;
        }

        let Some(parent) = requested.parent() else {
            return Err(format!(
                "--control-socket {} has no parent directory",
                requested.display()
            ));
        };
        let Some(file_name) = requested.file_name() else {
            return Err(format!(
                "--control-socket {} must name a socket file",
                requested.display()
            ));
        };

        let my_uid = rustix::process::geteuid().as_raw();
        let parent_sym = fs::symlink_metadata(parent).map_err(|error| {
            format!(
                "control socket parent directory {} cannot be inspected: {error} (create it first with mode 0700)",
                parent.display()
            )
        })?;
        if parent_sym.file_type().is_symlink() || !parent_sym.file_type().is_dir() {
            return Err(format!(
                "control socket parent {} must be a real directory (not a symlink or file)",
                parent.display()
            ));
        }
        if parent_sym.uid() != my_uid {
            return Err(format!(
                "control socket parent {} is owned by uid {} (expected {my_uid})",
                parent.display(),
                parent_sym.uid()
            ));
        }
        if (parent_sym.mode() & 0o077) != 0 {
            return Err(format!(
                "control socket parent {} has mode {:04o}; it must be private to this user (0700) so other local users cannot reach the socket",
                parent.display(),
                parent_sym.mode() & 0o7777
            ));
        }

        let canonical_parent = fs::canonicalize(parent).map_err(|error| {
            format!(
                "control socket parent {} cannot be canonicalized: {error}",
                parent.display()
            )
        })?;
        let canonical_meta = fs::metadata(&canonical_parent).map_err(|error| {
            format!(
                "control socket parent {} cannot be inspected: {error}",
                canonical_parent.display()
            )
        })?;
        if canonical_meta.uid() != my_uid || (canonical_meta.mode() & 0o077) != 0 {
            return Err(format!(
                "control socket parent {} must be owned by uid {my_uid} with mode 0700",
                canonical_parent.display()
            ));
        }

        // Never allow placing the control socket inside `agents/` or any agent workspace,
        // regardless of whether kernel confinement is active.
        let agents_dir = layout.agents_dir();
        let canonical_agents = fs::canonicalize(&agents_dir).unwrap_or(agents_dir);
        if canonical_parent == canonical_agents || canonical_parent.starts_with(&canonical_agents) {
            return Err(format!(
                "--control-socket {} lies inside the agents directory {}; place it outside agent workspaces (or omit --control-socket to use {}/{})",
                requested.display(),
                canonical_agents.display(),
                layout.state_root().display(),
                CONTROL_SOCKET_FILE
            ));
        }
        let workspaces = confinement::agent_workspaces(&layout.agents_dir());
        if confinement::layout_overlap(std::slice::from_ref(&canonical_parent), &workspaces)
            .is_some()
        {
            return Err(format!(
                "--control-socket {} overlaps an agent workspace; omit --control-socket to use {}/{}",
                requested.display(),
                layout.state_root().display(),
                CONTROL_SOCKET_FILE
            ));
        }

        // When runs are confined, ensure a custom `--control-socket` parent cannot be
        // reached from inside a confined agent shell.
        if custom.is_some() && view.confined {
            if view.confinement.as_deref() == Some(confinement::Method::Landlock.label()) {
                if confinement::landlock_overlap(
                    std::slice::from_ref(&canonical_parent),
                    &workspaces,
                )
                .is_some()
                {
                    return Err(format!(
                        "--control-socket directory {} lies inside a tree granted to confined agent shells by Landlock; omit --control-socket to use {}/{} or choose a private 0700 directory outside system/runtime/workspace paths",
                        canonical_parent.display(),
                        layout.state_root().display(),
                        CONTROL_SOCKET_FILE
                    ));
                }
            } else {
                let protected_roots: Vec<PathBuf> = [
                    Some(layout.root().to_path_buf()),
                    Some(layout.state_root().to_path_buf()),
                    Some(data_root.to_path_buf()),
                    Some(std::env::temp_dir()),
                ]
                .into_iter()
                .flatten()
                .filter_map(|p| fs::canonicalize(p).ok())
                .collect();
                let inside_hidden = protected_roots
                    .iter()
                    .any(|root| canonical_parent == *root || canonical_parent.starts_with(root));
                if !inside_hidden {
                    return Err(format!(
                        "--control-socket directory {} is not hidden by `{}` confinement; omit --control-socket to use {}/{} (or place it inside a 0700 subdirectory of state/temp)",
                        canonical_parent.display(),
                        view.confinement.as_deref().unwrap_or("namespace"),
                        layout.state_root().display(),
                        CONTROL_SOCKET_FILE
                    ));
                }
            }
        }

        let socket_path = canonical_parent.join(file_name);
        match fs::symlink_metadata(&socket_path) {
            Ok(existing) => {
                if !existing.file_type().is_socket() {
                    return Err(format!(
                        "control socket path {} already exists and is not a Unix socket; refusing to overwrite it",
                        socket_path.display()
                    ));
                }
                if existing.uid() != my_uid {
                    return Err(format!(
                        "stale control socket {} is owned by uid {} (expected {my_uid}); refusing to remove it",
                        socket_path.display(),
                        existing.uid()
                    ));
                }
                fs::remove_file(&socket_path).map_err(|error| {
                    format!(
                        "stale control socket {} cannot be removed: {error}",
                        socket_path.display()
                    )
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "control socket path {} cannot be inspected: {error}",
                    socket_path.display()
                ));
            }
        }

        let cookie_path = control_cookie_path(&socket_path)?;
        let (cookie, cookie_dev, cookie_ino) = write_control_cookie_file(&cookie_path, my_uid)?;

        let listener = match bind_unix_socket_at(&canonical_parent, file_name, &socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_file(&cookie_path);
                return Err(error);
            }
        };
        set_mode(&socket_path, SECRET_FILE_MODE).map_err(|error| {
            let _ = fs::remove_file(&socket_path);
            let _ = fs::remove_file(&cookie_path);
            format!(
                "control socket {} permissions cannot be set to 0600: {error}",
                socket_path.display()
            )
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            let _ = fs::remove_file(&socket_path);
            let _ = fs::remove_file(&cookie_path);
            format!(
                "control socket {} cannot be switched to non-blocking mode: {error}",
                socket_path.display()
            )
        })?;
        let sock_meta = fs::symlink_metadata(&socket_path).map_err(|error| {
            let _ = fs::remove_file(&socket_path);
            let _ = fs::remove_file(&cookie_path);
            format!(
                "control socket {} metadata cannot be verified: {error}",
                socket_path.display()
            )
        })?;
        if !sock_meta.file_type().is_socket() || (sock_meta.mode() & 0o777) != SECRET_FILE_MODE {
            let _ = fs::remove_file(&socket_path);
            let _ = fs::remove_file(&cookie_path);
            return Err(format!(
                "control socket {} did not retain mode 0600",
                socket_path.display()
            ));
        }

        Ok(Self {
            listener,
            path: socket_path,
            dev: sock_meta.dev(),
            ino: sock_meta.ino(),
            cookie,
            cookie_path,
            cookie_dev,
            cookie_ino,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cookie(&self) -> &str {
        &self.cookie
    }
}

impl Drop for ControlSocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
        if let Ok(meta) = fs::symlink_metadata(&self.cookie_path)
            && meta.file_type().is_file()
            && !meta.file_type().is_symlink()
            && meta.dev() == self.cookie_dev
            && meta.ino() == self.cookie_ino
        {
            let _ = fs::remove_file(&self.cookie_path);
        }
    }
}

fn control_cookie_path(socket_path: &Path) -> Result<PathBuf, String> {
    let Some(parent) = socket_path.parent() else {
        return Err(format!(
            "control socket path {} has no parent directory",
            socket_path.display()
        ));
    };
    let Some(file_name) = socket_path.file_name() else {
        return Err(format!(
            "control socket path {} has no filename",
            socket_path.display()
        ));
    };
    let mut cookie_name = file_name.to_os_string();
    cookie_name.push(".cookie");
    Ok(parent.join(cookie_name))
}

fn write_control_cookie_file(cookie_path: &Path, my_uid: u32) -> Result<(String, u64, u64), String> {
    match fs::symlink_metadata(cookie_path) {
        Ok(existing) => {
            if existing.file_type().is_symlink() || !existing.file_type().is_file() {
                return Err(format!(
                    "control cookie path {} already exists and is not a regular file; refusing to overwrite it",
                    cookie_path.display()
                ));
            }
            if existing.uid() != my_uid {
                return Err(format!(
                    "stale control cookie {} is owned by uid {} (expected {my_uid}); refusing to overwrite it",
                    cookie_path.display(),
                    existing.uid()
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "control cookie path {} cannot be inspected: {error}",
                cookie_path.display()
            ));
        }
    }

    let mut raw = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut raw))
        .map_err(|error| format!("failed to read random bytes for control cookie: {error}"))?;
    let mut cookie = String::with_capacity(64);
    for byte in raw {
        use std::fmt::Write as _;
        let _ = write!(&mut cookie, "{byte:02x}");
    }

    let mut stage_os = cookie_path.as_os_str().to_os_string();
    stage_os.push(format!(".part-{}", std::process::id()));
    let stage = PathBuf::from(stage_os);
    let write_res = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(SECRET_FILE_MODE)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&stage)?;
        file.write_all(cookie.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        set_mode(&stage, SECRET_FILE_MODE)?;
        fs::rename(&stage, cookie_path)
    })();
    if let Err(error) = write_res {
        let _ = fs::remove_file(&stage);
        return Err(format!(
            "control cookie {} cannot be written: {error}",
            cookie_path.display()
        ));
    }

    let meta = fs::symlink_metadata(cookie_path).map_err(|error| {
        let _ = fs::remove_file(cookie_path);
        format!(
            "control cookie {} metadata cannot be verified: {error}",
            cookie_path.display()
        )
    })?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != my_uid
        || (meta.mode() & 0o777) != SECRET_FILE_MODE
    {
        let _ = fs::remove_file(cookie_path);
        return Err(format!(
            "control cookie {} did not retain regular file mode 0600",
            cookie_path.display()
        ));
    }

    Ok((cookie, meta.dev(), meta.ino()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Bind a Unix domain socket even when `parent` exceeds Linux `sockaddr_un.sun_path`
/// (`SUN_LEN`, 107 bytes) by addressing `<file_name>` through `/proc/self/fd/<dir_fd>`.
fn bind_unix_socket_at(
    parent: &Path,
    file_name: &std::ffi::OsStr,
    socket_path: &Path,
) -> Result<UnixListener, String> {
    const SAFE_SUN_PATH_BYTES: usize = 100;
    const MAX_SUN_PATH_BYTES: usize = 107;

    if socket_path.as_os_str().len() <= SAFE_SUN_PATH_BYTES {
        return UnixListener::bind(socket_path).map_err(|error| {
            format!(
                "control socket {} cannot be bound: {error}",
                socket_path.display()
            )
        });
    }

    if Path::new("/proc/self/fd").is_dir() {
        let dir_file = fs::File::open(parent).map_err(|error| {
            format!(
                "control socket parent {} cannot be opened: {error}",
                parent.display()
            )
        })?;
        let proc_sock = PathBuf::from(format!("/proc/self/fd/{}", dir_file.as_raw_fd()))
            .join(file_name);
        if proc_sock.as_os_str().len() > MAX_SUN_PATH_BYTES {
            return Err(format!(
                "control socket filename in {} exceeds the Unix domain socket path limit ({MAX_SUN_PATH_BYTES} bytes); pass `--control-socket <short-0700-dir>/headless.sock`",
                socket_path.display()
            ));
        }
        return UnixListener::bind(&proc_sock).map_err(|error| {
            format!(
                "control socket {} cannot be bound: {error} (pass `--control-socket <short-0700-dir>/headless.sock` or use a shorter `--state-dir`)",
                socket_path.display()
            )
        });
    }

    UnixListener::bind(socket_path).map_err(|error| {
        format!(
            "control socket {} cannot be bound ({error}) — Unix domain socket paths are limited to {MAX_SUN_PATH_BYTES} bytes; pass `--control-socket <short-0700-dir>/headless.sock` or use a shorter `--state-dir`",
            socket_path.display()
        )
    })
}

fn connect_unix_socket_raw(socket_path: &Path) -> io::Result<UnixStream> {
    const SAFE_SUN_PATH_BYTES: usize = 100;
    const MAX_SUN_PATH_BYTES: usize = 107;

    if socket_path.as_os_str().len() <= SAFE_SUN_PATH_BYTES {
        return UnixStream::connect(socket_path);
    }
    if let (Some(parent), Some(file_name)) = (socket_path.parent(), socket_path.file_name())
        && Path::new("/proc/self/fd").is_dir()
    {
        let dir_file = fs::File::open(parent)?;
        let proc_sock = PathBuf::from(format!("/proc/self/fd/{}", dir_file.as_raw_fd()))
            .join(file_name);
        if proc_sock.as_os_str().len() > MAX_SUN_PATH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control socket filename exceeds Unix SUN_LEN (107 bytes); pass a shorter --control-socket path",
            ));
        }
        return UnixStream::connect(&proc_sock);
    }
    UnixStream::connect(socket_path)
}

/// Tracks agents armed to auto-park (`Command::Stop`) as soon as a session finishes
/// with `SessionEnd::NoWork` (before `NO_WORK_DELAY` schedules another LLM turn).
#[derive(Debug, Default)]
struct StopOnNoWorkTracker {
    armed_min_runs: HashMap<String, u64>,
}

impl StopOnNoWorkTracker {
    /// Arm an already-started agent (at `--headless --stop-on-no-work` startup or via
    /// daemon-wide `--control --stop-on-no-work`): if its most recent completed round is
    /// already `NoWork` (waiting in `NO_WORK_DELAY`), park immediately without an extra run.
    fn arm_existing(&mut self, agent: &crate::engine_api::AgentView) {
        let min_runs = if matches!(
            &agent.session,
            SessionState::Running { .. }
                | SessionState::Restarting {
                    last: SessionEnd::NoWork { .. },
                    ..
                }
        ) {
            agent.runs_total
        } else {
            agent.runs_total.saturating_add(1)
        };
        self.armed_min_runs.insert(agent.name.clone(), min_runs);
    }

    /// Arm an agent being explicitly started/woken now (`--control --start-agent` /
    /// `--start-all`), ensuring at least one fresh run executes unless one is already live.
    fn arm_for_fresh_wake(&mut self, agent: &crate::engine_api::AgentView) {
        let min_runs = if matches!(agent.session, SessionState::Running { .. }) {
            agent.runs_total
        } else {
            agent.runs_total.saturating_add(1)
        };
        self.armed_min_runs.insert(agent.name.clone(), min_runs);
    }

    fn disarm(&mut self, name: &str) {
        self.armed_min_runs.remove(name);
    }

    fn clear(&mut self) {
        self.armed_min_runs.clear();
    }

    fn is_armed(&self, name: &str) -> bool {
        self.armed_min_runs.contains_key(name)
    }

    fn reconcile_after_tick(&mut self, engine: &mut Engine) -> Vec<String> {
        let view = engine.board();
        let mut to_park = Vec::new();
        for agent in &view.agents {
            if agent.desired != Desired::Started {
                self.armed_min_runs.remove(&agent.name);
                continue;
            }
            let Some(&min_runs) = self.armed_min_runs.get(&agent.name) else {
                continue;
            };
            if agent.runs_total >= min_runs
                && matches!(
                    &agent.session,
                    SessionState::Restarting {
                        last: SessionEnd::NoWork { .. },
                        ..
                    }
                )
            {
                to_park.push((agent.name.clone(), agent.runs_total));
            }
        }

        let mut parked = Vec::new();
        for (name, runs_total) in to_park {
            self.armed_min_runs.remove(&name);
            match engine.send(Command::Stop { name: name.clone() }) {
                Ok(()) => {
                    log_daemon(&format!(
                        "agent `{name}` finished with no remaining work; parked to stopped (total runs {runs_total})"
                    ));
                    parked.push(name);
                }
                Err(error) => {
                    log_daemon(&format!(
                        "warning: agent `{name}` could not be parked after no-work: {error}"
                    ));
                }
            }
        }
        parked
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlRequest {
    #[serde(default)]
    auth_cookie: String,
    #[serde(default)]
    start_agents: Vec<String>,
    #[serde(default)]
    stop_agents: Vec<String>,
    #[serde(default)]
    start_all: bool,
    #[serde(default)]
    stop_all: bool,
    #[serde(default)]
    stop_on_no_work: bool,
    #[serde(default)]
    keep_running: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status: Option<StatusReport>,
}

fn validate_headless_preconditions(
    layout: &Layout,
    view: &BoardView,
    allow_idle: bool,
) -> Result<(), String> {
    if !view.terms_acknowledged {
        return Err(
            "first-run terms have not been acknowledged; run `gewu-multiverse --configure --accept-terms` (or pass `--accept-terms`) first"
                .to_owned(),
        );
    }

    let queued: Vec<&str> = view
        .agents
        .iter()
        .filter(|a| engine::removal_requested(layout, &a.name))
        .map(|a| a.name.as_str())
        .collect();

    let started: Vec<_> = view
        .agents
        .iter()
        .filter(|agent| agent.desired == Desired::Started)
        .collect();

    if started.is_empty() {
        if let Some(first) = queued.first() {
            return Err(format!(
                "no agent is set to start (pending removal for `{}`: run `gewu-multiverse --configure --remove-agent {first}` to finish revoking its Forgejo sign-in, and start at least one agent with `--start-agent <name>`)",
                queued.join(", ")
            ));
        }
        if !allow_idle {
            return Err(
                "no agent is set to start; configure and start at least one agent with `gewu-multiverse --configure ... --start-agent <name>` (or pass `--allow-idle` to keep bundled Forgejo warm with zero idle model calls)"
                    .to_owned(),
            );
        }
        if view.agents.is_empty() {
            return Err(
                "no agents are configured; create at least one agent first with `gewu-multiverse --configure --accept-terms --agents 1 ...`"
                    .to_owned(),
            );
        }
    }

    if let Some(first) = queued.first() {
        return Err(format!(
            "agent `{first}` is queued for removal; finish removing it with `gewu-multiverse --configure --remove-agent {first}`"
        ));
    }

    for agent in started {
        // Recovery blocks this agent in the engine; keep the daemon available
        // for the other agents and for an explicit retry on this one.
        if agent.leftover.is_some() {
            continue;
        }
        if let Some(err) = &agent.program_error {
            return Err(format!(
                "agent `{}` cannot start: {err} — {}",
                agent.name,
                repair_command_for_agent_block(&agent.name, &agent.program, err)
            ));
        }
        if agent.instruction_chars == 0 {
            return Err(format!(
                "agent `{}` has an empty instruction; set one with `gewu-multiverse --configure --agent {} --instruction-file <file>`",
                agent.name, agent.name
            ));
        }
    }
    Ok(())
}

fn log_daemon(line: &str) {
    let _ = writeln!(io::stderr(), "gewu-multiverse[headless]: {line}");
    let _ = io::stderr().flush();
}

/// Run the real engine as a headless daemon until `SIGINT`, `SIGTERM`, or `SIGHUP`.
pub fn run_headless(cli: HeadlessCli) -> Result<(), String> {
    let signals = DaemonSignals::install()?;
    let (layout, data_root) = resolve_paths(cli.root, cli.state_root, cli.data_root)?;
    let state_root = layout.state_root().to_path_buf();
    let options = EngineOptions::new(layout.clone(), &data_root)?;
    let stop_grace = options.stop_grace;
    let mut engine = engine::open_with(options)?;

    let outcome = (|| -> Result<(), String> {
        let mut view = engine.board();
        if let Some(upgrade) = &view.upgrade {
            log_daemon(&format!(
                "upgraded installation from {} to {} (kept {} agent(s), {} run(s))",
                upgrade.from_version, upgrade.to_version, upgrade.agents, upgrade.runs
            ));
        }
        if cli.accept_terms && !view.terms_acknowledged {
            for (idx, line) in view.terms.iter().enumerate() {
                log_daemon(&format!("term {}: {line}", idx + 1));
            }
            engine.send(Command::AcknowledgeTerms)?;
            view = engine.board();
            log_daemon(&format!("acknowledged terms (version {TERMS_VERSION})"));
        }

        validate_headless_preconditions(&layout, &view, cli.allow_idle)?;

        let control_guard =
            ControlSocketGuard::bind(&layout, &data_root, cli.control_socket.as_deref(), &view)?;
        let control_socket_str = control_guard.path().display().to_string();

        let mut daemon_stop_on_no_work = cli.stop_on_no_work;
        let mut auto_park = StopOnNoWorkTracker::default();
        if daemon_stop_on_no_work {
            for agent in &view.agents {
                if agent.desired == Desired::Started {
                    auto_park.arm_existing(agent);
                }
            }
        }

        let started_names: Vec<String> = view
            .agents
            .iter()
            .filter(|a| a.desired == Desired::Started)
            .map(|a| a.name.clone())
            .collect();
        let started_desc = if started_names.is_empty() {
            "none (idle-warm standby)".to_owned()
        } else {
            started_names.join(", ")
        };
        let confine_label = view
            .confinement
            .clone()
            .unwrap_or_else(|| "none (runs are not confined on this computer)".to_owned());
        log_daemon(&format!(
            "starting headless daemon (pid {}, confinement: {confine_label}, control socket: {}, started agents: {started_desc})",
            std::process::id(),
            control_guard.path().display(),
        ));

        let mut forgejo_ever_ran = matches!(view.forgejo.state, ForgejoState::Running);
        let mut last_forgejo_line = String::new();
        let mut last_agent_lines: HashMap<String, String> = HashMap::new();
        let mut last_snapshot_sig = String::new();
        let mut last_snapshot_written_at: Option<Instant> = None;

        loop {
            if signals.requested() {
                log_daemon("termination signal received; shutting down");
                break;
            }
            let _ = drain_control_requests(
                &control_guard,
                &layout,
                &data_root,
                &mut engine,
                &mut auto_park,
                &mut daemon_stop_on_no_work,
                &state_root,
                &mut last_snapshot_sig,
                &mut last_snapshot_written_at,
            );

            engine.tick();
            let parked = auto_park.reconcile_after_tick(&mut engine);
            if !parked.is_empty() {
                for name in parked {
                    last_agent_lines.remove(&name);
                }
            }
            let view = engine.board();

            let forgejo_line = describe_forgejo_state(&view.forgejo.state, &view.forgejo.url);
            if forgejo_line != last_forgejo_line {
                log_daemon(&format!("forgejo: {forgejo_line}"));
                last_forgejo_line = forgejo_line;
            }
            if matches!(view.forgejo.state, ForgejoState::Running) {
                forgejo_ever_ran = true;
            } else if !forgejo_ever_ran
                && let ForgejoState::Failed { reason } = &view.forgejo.state
            {
                return Err(format!(
                    "bundled Forgejo failed to start: {reason} — install or repair Forgejo with `gewu-multiverse --install-forgejo` (or `sh install.sh --tarball <release.tar.gz>` offline)"
                ));
            }

            for agent in &view.agents {
                if agent.desired != Desired::Started {
                    last_agent_lines.remove(&agent.name);
                    continue;
                }
                let line = describe_agent_runtime(agent);
                if last_agent_lines.get(&agent.name) != Some(&line) {
                    log_daemon(&format!("agent `{}`: {line}", agent.name));
                    last_agent_lines.insert(agent.name.clone(), line);
                }
            }

            let snap = build_snapshot_from_view(
                &layout,
                &data_root,
                &view,
                true,
                true,
                daemon_stop_on_no_work,
                &auto_park,
                &control_socket_str,
            );
            maybe_write_snapshot(
                &state_root,
                &snap,
                &mut last_snapshot_sig,
                &mut last_snapshot_written_at,
            );

            for _ in 0..10 {
                if signals.requested() {
                    break;
                }
                if drain_control_requests(
                    &control_guard,
                    &layout,
                    &data_root,
                    &mut engine,
                    &mut auto_park,
                    &mut daemon_stop_on_no_work,
                    &state_root,
                    &mut last_snapshot_sig,
                    &mut last_snapshot_written_at,
                ) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        Ok(())
    })();

    engine.shutdown(stop_grace);
    let final_socket = resolve_control_socket_path(&layout, cli.control_socket.as_deref())
        .unwrap_or_else(|_| state_root.join(CONTROL_SOCKET_FILE));
    let final_view = engine.board();
    let empty_tracker = StopOnNoWorkTracker::default();
    let final_snap = build_snapshot_from_view(
        &layout,
        &data_root,
        &final_view,
        false,
        false,
        false,
        &empty_tracker,
        &final_socket.display().to_string(),
    );
    if let Ok(json) = serde_json::to_string_pretty(&final_snap) {
        let _ = write_snapshot_file(&state_root, &json);
    }
    if outcome.is_ok() {
        log_daemon("stopped every started agent and Forgejo cleanly");
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn drain_control_requests(
    guard: &ControlSocketGuard,
    layout: &Layout,
    data_root: &Path,
    engine: &mut Engine,
    auto_park: &mut StopOnNoWorkTracker,
    daemon_stop_on_no_work: &mut bool,
    state_root: &Path,
    last_snapshot_sig: &mut String,
    last_snapshot_written_at: &mut Option<Instant>,
) -> bool {
    let mut handled_any = false;
    for _ in 0..16 {
        let stream = match guard.listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                log_daemon(&format!("warning: control socket accept error: {error}"));
                break;
            }
        };
        handled_any = true;
        handle_one_control_stream(
            stream,
            guard.path(),
            guard.cookie(),
            layout,
            data_root,
            engine,
            auto_park,
            daemon_stop_on_no_work,
            state_root,
            last_snapshot_sig,
            last_snapshot_written_at,
        );
    }
    handled_any
}

#[allow(clippy::too_many_arguments)]
fn handle_one_control_stream(
    mut stream: UnixStream,
    socket_path: &Path,
    expected_cookie: &str,
    layout: &Layout,
    data_root: &Path,
    engine: &mut Engine,
    auto_park: &mut StopOnNoWorkTracker,
    daemon_stop_on_no_work: &mut bool,
    state_root: &Path,
    last_snapshot_sig: &mut String,
    last_snapshot_written_at: &mut Option<Instant>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(SERVER_SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SERVER_SOCKET_TIMEOUT));

    let my_uid = rustix::process::geteuid();
    match rustix::net::sockopt::get_socket_peercred(&stream) {
        Ok(cred) if cred.uid == my_uid => {}
        Ok(cred) => {
            let resp = ControlResponse {
                ok: false,
                actions: Vec::new(),
                error: Some(format!(
                    "control socket rejected peer uid {} (expected {})",
                    cred.uid.as_raw(),
                    my_uid.as_raw()
                )),
                status: None,
            };
            let _ = write_control_response(&mut stream, &resp);
            return;
        }
        Err(error) => {
            let resp = ControlResponse {
                ok: false,
                actions: Vec::new(),
                error: Some(format!("control socket peer credentials check failed: {error}")),
                status: None,
            };
            let _ = write_control_response(&mut stream, &resp);
            return;
        }
    }

    let mut line = String::new();
    let read_res = {
        let limited = (&mut stream).take(MAX_CONTROL_REQUEST_BYTES + 1);
        let mut reader = BufReader::new(limited);
        reader.read_line(&mut line)
    };
    if let Err(error) = read_res {
        let resp = ControlResponse {
            ok: false,
            actions: Vec::new(),
            error: Some(format!("failed to read control request: {error}")),
            status: None,
        };
        let _ = write_control_response(&mut stream, &resp);
        return;
    }
    if line.is_empty() {
        return;
    }
    if line.len() as u64 > MAX_CONTROL_REQUEST_BYTES {
        let resp = ControlResponse {
            ok: false,
            actions: Vec::new(),
            error: Some(format!(
                "control request exceeds maximum size ({MAX_CONTROL_REQUEST_BYTES} bytes)"
            )),
            status: None,
        };
        let _ = write_control_response(&mut stream, &resp);
        return;
    }

    let req: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(req) => req,
        Err(error) => {
            let resp = ControlResponse {
                ok: false,
                actions: Vec::new(),
                error: Some(format!("invalid control request JSON: {error}")),
                status: None,
            };
            let _ = write_control_response(&mut stream, &resp);
            return;
        }
    };

    if !constant_time_eq(req.auth_cookie.trim().as_bytes(), expected_cookie.as_bytes()) {
        let resp = ControlResponse {
            ok: false,
            actions: Vec::new(),
            error: Some(
                "unauthorized control socket request: missing or invalid daemon control cookie"
                    .to_owned(),
            ),
            status: None,
        };
        let _ = write_control_response(&mut stream, &resp);
        return;
    }

    let apply_result = apply_control_request(
        &req,
        layout,
        engine,
        auto_park,
        daemon_stop_on_no_work,
    );

    let view = engine.board();
    let snap = build_snapshot_from_view(
        layout,
        data_root,
        &view,
        true,
        true,
        *daemon_stop_on_no_work,
        auto_park,
        &socket_path.display().to_string(),
    );
    maybe_write_snapshot(
        state_root,
        &snap,
        last_snapshot_sig,
        last_snapshot_written_at,
    );

    let resp = match apply_result {
        Ok(actions) => ControlResponse {
            ok: true,
            actions,
            error: None,
            status: Some(snap),
        },
        Err(error) => ControlResponse {
            ok: false,
            actions: Vec::new(),
            error: Some(error),
            status: Some(snap),
        },
    };
    let _ = write_control_response(&mut stream, &resp);
}

fn maybe_write_snapshot(
    state_root: &Path,
    snap: &StatusReport,
    last_sig: &mut String,
    last_written_at: &mut Option<Instant>,
) {
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
    let mut normalized = snap.clone();
    normalized.updated_unix_ms = 0;
    let Ok(sig) = serde_json::to_string(&normalized) else {
        return;
    };
    let heartbeat_due = last_written_at.is_none_or(|at| at.elapsed() >= HEARTBEAT_INTERVAL);
    if sig == *last_sig && !heartbeat_due {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(snap)
        && write_snapshot_file(state_root, &json).is_ok()
    {
        *last_sig = sig;
        *last_written_at = Some(Instant::now());
    }
}

fn write_control_response(stream: &mut UnixStream, resp: &ControlResponse) -> Result<(), String> {
    let mut payload =
        serde_json::to_string(resp).map_err(|e| format!("response JSON failed: {e}"))?;
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("response write failed: {e}"))
}

fn apply_control_request(
    req: &ControlRequest,
    layout: &Layout,
    engine: &mut Engine,
    auto_park: &mut StopOnNoWorkTracker,
    daemon_stop_on_no_work: &mut bool,
) -> Result<Vec<String>, String> {
    if req.start_all && req.stop_all {
        return Err("cannot pass both `--start-all` and `--stop-all`".to_owned());
    }
    if req.stop_on_no_work && req.keep_running {
        return Err("pass either `--stop-on-no-work` or `--keep-running`, not both".to_owned());
    }

    // Refresh `instruction.md` sizes/contents from disk first so newly written instructions
    // take effect immediately on the very next started session.
    engine.refresh_instructions();
    let view = engine.board();
    if view.agents.is_empty() {
        return Err("no agents are configured on the running daemon".to_owned());
    }

    let mut to_stop = req.stop_agents.clone();
    if req.stop_all {
        for agent in &view.agents {
            if !engine::removal_requested(layout, &agent.name) && !to_stop.contains(&agent.name) {
                to_stop.push(agent.name.clone());
            }
        }
    }

    let mut to_start = req.start_agents.clone();
    if req.start_all {
        for agent in &view.agents {
            if !to_start.contains(&agent.name) {
                to_start.push(agent.name.clone());
            }
        }
    }

    for name in &to_start {
        if to_stop.contains(name) {
            return Err(format!(
                "agent `{name}` cannot be both started and stopped in the same `--control` command"
            ));
        }
    }

    // Validate every target name and every start precondition atomically BEFORE mutating
    // any agent's state on the running daemon.
    for stop_name in &to_stop {
        if !view.agents.iter().any(|a| a.name == *stop_name) {
            return Err(format!("there is no agent named `{stop_name}`"));
        }
        if !req.stop_all && engine::removal_requested(layout, stop_name) {
            return Err(format!(
                "agent `{stop_name}` is queued for removal; its Forgejo sign-in is already being revoked"
            ));
        }
    }
    for start_name in &to_start {
        let Some(agent_view) = view.agents.iter().find(|a| a.name == *start_name) else {
            return Err(format!("there is no agent named `{start_name}`"));
        };
        if engine::removal_requested(layout, start_name) {
            return Err(format!(
                "agent `{start_name}` is queued for removal and cannot be started"
            ));
        }
        if let Some(leftover) = &agent_view.leftover
            && (to_start.len() != 1 || !to_stop.is_empty())
        {
            return Err(format!(
                "agent `{start_name}` needs a separate --control --start-agent {start_name} to retry cleanup ({leftover})"
            ));
        }
        if let Some(err) = &agent_view.program_error {
            return Err(format!(
                "agent `{start_name}` cannot start: {err} — stop the daemon first, then {}",
                repair_command_for_agent_block(start_name, &agent_view.program, err)
            ));
        }
        if agent_view.instruction_chars == 0 {
            let inst_hint = layout
                .agent(start_name)
                .map(|p| p.instruction().display().to_string())
                .unwrap_or_else(|_| format!("<config-dir>/agents/{start_name}/instruction.md"));
            return Err(format!(
                "agent `{start_name}` cannot start because its instruction is empty — write `{inst_hint}` first, then re-run `gewu-multiverse --control --start-agent {start_name}`"
            ));
        }
    }

    let mut actions = Vec::new();

    // Execute `to_start` first with full rollback of any newly started targets if a
    // runtime-only check (such as `codex_sandbox_block`) rejects a later agent in the
    // same batch, so neither `to_start` nor `to_stop` leaves a partial mutation.
    if !to_start.is_empty() {
        let saved_auto_park = auto_park.armed_min_runs.clone();
        let mut newly_started: Vec<String> = Vec::new();
        let effective_auto_park = if req.keep_running {
            false
        } else if req.stop_on_no_work {
            true
        } else {
            *daemon_stop_on_no_work
        };

        for start_name in &to_start {
            let was_stopped = view
                .agents
                .iter()
                .find(|a| a.name == *start_name)
                .is_some_and(|a| a.desired == Desired::Stopped);
            if let Some(before) = view.agents.iter().find(|a| a.name == *start_name) {
                if effective_auto_park {
                    auto_park.arm_for_fresh_wake(before);
                } else {
                    auto_park.disarm(start_name);
                }
            }
            if let Err(error) = engine.send(Command::Start {
                name: start_name.clone(),
            }) {
                auto_park.armed_min_runs = saved_auto_park;
                for prev in newly_started {
                    let _ = engine.send(Command::Stop { name: prev });
                }
                return Err(error);
            }
            let after = engine.board();
            if let Some(SessionState::Blocked { reason }) = after
                .agents
                .iter()
                .find(|a| a.name == *start_name)
                .map(|a| &a.session)
            {
                let reason = reason.clone();
                auto_park.armed_min_runs = saved_auto_park;
                let _ = engine.send(Command::Stop {
                    name: start_name.clone(),
                });
                for prev in newly_started {
                    let _ = engine.send(Command::Stop { name: prev });
                }
                return Err(format!("agent `{start_name}` cannot start: {reason}"));
            }
            if was_stopped {
                newly_started.push(start_name.clone());
            }
            let suffix = if effective_auto_park {
                " (stop-on-no-work)"
            } else {
                ""
            };
            log_daemon(&format!("control: started agent `{start_name}`{suffix}"));
            actions.push(format!("started agent `{start_name}`{suffix}"));
        }
    }

    for stop_name in &to_stop {
        auto_park.disarm(stop_name);
        engine.send(Command::Stop {
            name: stop_name.clone(),
        })?;
        log_daemon(&format!("control: stopped agent `{stop_name}`"));
        actions.push(format!("stopped agent `{stop_name}`"));
    }

    // Global daemon auto-park toggle when neither `--start-agent` nor `--start-all` was specified.
    if to_start.is_empty() {
        if req.stop_on_no_work {
            *daemon_stop_on_no_work = true;
            let current = engine.board();
            for agent in &current.agents {
                if agent.desired == Desired::Started {
                    auto_park.arm_existing(agent);
                }
            }
            let _ = auto_park.reconcile_after_tick(engine);
            log_daemon("control: enabled --stop-on-no-work for running daemon");
            actions.push("enabled stop-on-no-work".to_owned());
        } else if req.keep_running {
            *daemon_stop_on_no_work = false;
            auto_park.clear();
            log_daemon("control: disabled --stop-on-no-work (keep-running)");
            actions.push("disabled stop-on-no-work (keep-running)".to_owned());
        }
    } else {
        // Advance one engine tick immediately so newly started agents queue their session
        // before `run_control` returns to the caller.
        engine.tick();
        let _ = auto_park.reconcile_after_tick(engine);
    }

    Ok(actions)
}

/// Send a live start/stop command to a running `--headless` daemon over its private Unix
/// socket without acquiring `engine.lock` or restarting bundled Forgejo.
pub fn run_control(cli: ControlCli) -> Result<String, String> {
    if cli.start_all && cli.stop_all {
        return Err("cannot pass both `--start-all` and `--stop-all`".to_owned());
    }
    if cli.stop_on_no_work && cli.keep_running {
        return Err("pass either `--stop-on-no-work` or `--keep-running`, not both".to_owned());
    }
    for name in &cli.start_agents {
        if cli.stop_agents.contains(name) {
            return Err(format!(
                "agent `{name}` cannot be both started and stopped in the same `--control` command"
            ));
        }
    }

    let (layout, data_root) = resolve_paths(cli.root, cli.state_root, cli.data_root)?;
    let socket_path = resolve_control_socket_path(&layout, cli.control_socket.as_deref())?;

    let mut stream = connect_control_socket(&socket_path, &data_root)?;
    let _ = stream.set_read_timeout(Some(CLIENT_SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_SOCKET_TIMEOUT));

    let cookie_path = control_cookie_path(&socket_path)?;
    let auth_cookie = fs::read_to_string(&cookie_path)
        .map_err(|error| {
            format!(
                "control socket cookie {} cannot be read ({error}) — `--control` must be run by the daemon's host user outside confined agent shells",
                cookie_path.display()
            )
        })?
        .trim()
        .to_owned();
    if auth_cookie.is_empty() {
        return Err(format!(
            "control socket cookie {} is empty",
            cookie_path.display()
        ));
    }

    let req = ControlRequest {
        auth_cookie,
        start_agents: cli.start_agents,
        stop_agents: cli.stop_agents,
        start_all: cli.start_all,
        stop_all: cli.stop_all,
        stop_on_no_work: cli.stop_on_no_work,
        keep_running: cli.keep_running,
    };
    let mut payload =
        serde_json::to_string(&req).map_err(|e| format!("control request JSON failed: {e}"))?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).map_err(|error| {
        format!(
            "failed to send command to control socket {}: {error}",
            socket_path.display()
        )
    })?;
    stream.flush().map_err(|error| {
        format!(
            "failed to flush command to control socket {}: {error}",
            socket_path.display()
        )
    })?;

    let mut reply_line = String::new();
    {
        let limited = (&mut stream).take(MAX_CONTROL_RESPONSE_BYTES + 1);
        let mut reader = BufReader::new(limited);
        reader.read_line(&mut reply_line).map_err(|error| {
            format!(
                "failed to read reply from control socket {}: {error}",
                socket_path.display()
            )
        })?;
    }
    if reply_line.trim().is_empty() {
        return Err(format!(
            "headless daemon closed control socket {} without a response",
            socket_path.display()
        ));
    }
    if reply_line.len() as u64 > MAX_CONTROL_RESPONSE_BYTES {
        return Err("control socket response exceeded maximum size".to_owned());
    }

    let resp: ControlResponse = serde_json::from_str(reply_line.trim()).map_err(|error| {
        format!(
            "invalid response from control socket {}: {error}",
            socket_path.display()
        )
    })?;

    if !resp.ok {
        return Err(resp
            .error
            .unwrap_or_else(|| "control command was rejected by headless daemon".to_owned()));
    }

    if cli.json {
        return serde_json::to_string_pretty(&resp)
            .map_err(|error| format!("control JSON serialization failed: {error}"));
    }

    if resp.actions.is_empty() {
        Ok("gewu-multiverse: control command applied".to_owned())
    } else {
        Ok(resp
            .actions
            .iter()
            .map(|line| format!("gewu-multiverse: {line}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn connect_control_socket(socket_path: &Path, data_root: &Path) -> Result<UnixStream, String> {
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match connect_unix_socket_raw(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                let lock_held = engine_lock_held(data_root);
                if lock_held
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    )
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if lock_held && error.kind() == io::ErrorKind::NotFound {
                    return Err(format!(
                        "an active gewu-multiverse instance holds the engine lock, but no headless control socket exists at {} (if the interactive TUI board is open, use the board directly or run `gewu-multiverse --headless`)",
                        socket_path.display()
                    ));
                }
                return Err(format!(
                    "no running `--headless` daemon is listening at {} ({error}) — start one with `gewu-multiverse --headless --allow-idle` (or use `gewu-multiverse --configure` while stopped)",
                    socket_path.display()
                ));
            }
        }
    }
}

fn write_snapshot_file(state_root: &Path, json: &str) -> Result<(), String> {
    fs::create_dir_all(state_root)
        .map_err(|error| format!("{} cannot be created: {error}", state_root.display()))?;
    let path = state_root.join(STATUS_SNAPSHOT_FILE);
    crate::mode::write_private(&path, json.as_bytes())
        .map_err(|error| format!("{} cannot be written: {error}", path.display()))
}

fn describe_forgejo_state(state: &ForgejoState, url: &str) -> String {
    match state {
        ForgejoState::Missing => "missing".to_owned(),
        ForgejoState::Fetching { percent: Some(p) } => format!("fetching ({p}%)"),
        ForgejoState::Fetching { percent: None } => "fetching".to_owned(),
        ForgejoState::Preparing => "preparing".to_owned(),
        ForgejoState::Starting => "starting".to_owned(),
        ForgejoState::Running => format!("running at {url}"),
        ForgejoState::Stopped => "stopped".to_owned(),
        ForgejoState::Failed { reason } => format!("failed — {reason}"),
    }
}

fn describe_session_end(end: &SessionEnd) -> String {
    match end {
        SessionEnd::Succeeded => "succeeded".to_owned(),
        SessionEnd::NoWork { reason } => format!("no-work ({reason})"),
        SessionEnd::Refused { reason } => format!("refused ({reason})"),
        SessionEnd::Warned { reason } => format!("warned ({reason})"),
        SessionEnd::Failed { reason } => format!("failed ({reason})"),
        SessionEnd::TimedOut => "timed-out".to_owned(),
        SessionEnd::Stopped { reason, .. } => format!("stopped ({reason})"),
    }
}

fn describe_agent_runtime(agent: &crate::engine_api::AgentView) -> String {
    let prov = match &agent.provisioning {
        Provisioning::Pending => "provisioning=pending".to_owned(),
        Provisioning::Ready => format!("login={}", agent.login),
        Provisioning::Failed { reason } => format!("provisioning-failed={reason}"),
    };
    let sess = match &agent.session {
        SessionState::Idle => "idle".to_owned(),
        SessionState::Running { number, .. } => {
            format!("running (session #{number}, total runs {})", agent.runs_total)
        }
        SessionState::Restarting { last, .. } => {
            format!(
                "waiting-next-run (last: {}, total runs {})",
                describe_session_end(last),
                agent.runs_total
            )
        }
        SessionState::Blocked { reason } => format!("blocked ({reason})"),
    };
    format!("{prov}, {sess}")
}

/// Non-secret status report emitted by `--status`, `--control --json`, and `headless-status.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub version: String,
    pub updated_unix_ms: u64,
    pub engine_running: bool,
    pub daemon_pid: Option<u32>,
    pub ready_for_headless: bool,
    #[serde(default)]
    pub ready_for_idle_headless: bool,
    pub healthy: bool,
    #[serde(default)]
    pub idle_standby: bool,
    #[serde(default)]
    pub stop_on_no_work: bool,
    #[serde(default)]
    pub control_socket: String,
    pub terms_acknowledged: bool,
    pub terms_version: u32,
    pub required_terms_version: u32,
    pub confined: bool,
    pub confinement: String,
    pub config_dir: String,
    pub state_dir: String,
    pub data_dir: String,
    pub forgejo: ForgejoStatusSummary,
    pub default_endpoint: EndpointStatusSummary,
    pub agents: Vec<AgentStatusSummary>,
    pub blocking_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgejoStatusSummary {
    pub binary_installed: bool,
    pub version: String,
    pub state: String,
    pub url: String,
    pub admin_login: String,
    pub repositories: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointStatusSummary {
    pub configured: bool,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    pub key_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusSummary {
    pub name: String,
    pub login: String,
    pub desired: String,
    #[serde(default)]
    pub stop_on_no_work: bool,
    pub program: String,
    pub program_installed: bool,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    pub key_present: bool,
    pub instruction_chars: usize,
    pub provisioning: String,
    pub session: String,
    pub runs_total: u64,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub program_error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot_from_view(
    layout: &Layout,
    data_root: &Path,
    view: &BoardView,
    running: bool,
    allow_idle_runtime: bool,
    daemon_stop_on_no_work: bool,
    auto_park: &StopOnNoWorkTracker,
    control_socket: &str,
) -> StatusReport {
    let blocking = collect_view_blocking_reasons(layout, view, allow_idle_runtime);
    let strict_blocking = collect_view_blocking_reasons(layout, view, false);
    let idle_blocking = collect_view_blocking_reasons(layout, view, true);
    let started_count = view
        .agents
        .iter()
        .filter(|a| a.desired == Desired::Started)
        .count();
    let idle_standby = running && started_count == 0 && !view.agents.is_empty();

    let forgejo_state = match &view.forgejo.state {
        ForgejoState::Missing => "missing".to_owned(),
        ForgejoState::Fetching { .. } => "fetching".to_owned(),
        ForgejoState::Preparing => "preparing".to_owned(),
        ForgejoState::Starting => "starting".to_owned(),
        ForgejoState::Running => "running".to_owned(),
        ForgejoState::Stopped => "stopped".to_owned(),
        ForgejoState::Failed { reason } => format!("failed: {reason}"),
    };
    let agents: Vec<AgentStatusSummary> = view
        .agents
        .iter()
        .map(|a| AgentStatusSummary {
            name: a.name.clone(),
            login: a.login.clone(),
            desired: match a.desired {
                Desired::Started => "started".to_owned(),
                Desired::Stopped => "stopped".to_owned(),
            },
            stop_on_no_work: a.desired == Desired::Started && auto_park.is_armed(&a.name),
            program: a.program.clone(),
            program_installed: a.program_installed,
            protocol: a.protocol.clone(),
            base_url: a.base_url.clone(),
            model: a.model.clone(),
            key_present: a.key_present,
            instruction_chars: a.instruction_chars,
            provisioning: match &a.provisioning {
                Provisioning::Pending => "pending".to_owned(),
                Provisioning::Ready => "ready".to_owned(),
                Provisioning::Failed { reason } => format!("failed: {reason}"),
            },
            session: match &a.session {
                SessionState::Idle => "idle".to_owned(),
                SessionState::Running { number, .. } => format!("running (#{number})"),
                SessionState::Restarting { last, .. } => {
                    format!("restarting ({})", describe_session_end(last))
                }
                SessionState::Blocked { reason } => format!("blocked: {reason}"),
            },
            runs_total: a.runs_total,
            last_result: a.last_result.clone(),
            last_error: a.last_error.clone(),
            program_error: a.program_error.clone(),
        })
        .collect();
    let forgejo_ready = matches!(view.forgejo.state, ForgejoState::Running)
        && view.forgejo.repositories >= 1
        && view.agents.iter().all(|a| {
            engine::removal_requested(layout, &a.name)
                || matches!(a.provisioning, Provisioning::Ready)
        });
    let healthy = running && blocking.is_empty() && forgejo_ready;
    StatusReport {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        updated_unix_ms: now_unix_ms(),
        engine_running: running,
        daemon_pid: running.then(std::process::id),
        ready_for_headless: strict_blocking.is_empty(),
        ready_for_idle_headless: idle_blocking.is_empty(),
        healthy,
        idle_standby,
        stop_on_no_work: daemon_stop_on_no_work,
        control_socket: control_socket.to_owned(),
        terms_acknowledged: view.terms_acknowledged,
        terms_version: if view.terms_acknowledged {
            TERMS_VERSION
        } else {
            0
        },
        required_terms_version: TERMS_VERSION,
        confined: view.confined,
        confinement: view
            .confinement
            .clone()
            .unwrap_or_else(|| "none".to_owned()),
        config_dir: layout.root().display().to_string(),
        state_dir: layout.state_root().display().to_string(),
        data_dir: data_root.display().to_string(),
        forgejo: ForgejoStatusSummary {
            binary_installed: is_forgejo_installed_cached(data_root),
            version: view.forgejo.version.clone(),
            state: forgejo_state,
            url: view.forgejo.url.clone(),
            admin_login: view.forgejo.admin_login.clone(),
            repositories: view.forgejo.repositories,
        },
        default_endpoint: EndpointStatusSummary {
            configured: view.endpoint.configured,
            protocol: view.endpoint.protocol.clone(),
            base_url: view.endpoint.base_url.clone(),
            model: view.endpoint.model.clone(),
            key_present: view.endpoint.key_present,
        },
        agents,
        blocking_reasons: blocking,
    }
}

fn is_forgejo_installed_cached(data_root: &Path) -> bool {
    struct CachedDigest {
        path: PathBuf,
        dev: u64,
        ino: u64,
        len: u64,
        mtime: i64,
        mtime_nsec: i64,
        verified: bool,
    }
    static CACHE: std::sync::Mutex<Option<CachedDigest>> = std::sync::Mutex::new(None);

    let bin_path = bundled_forgejo::binary_path(data_root);
    let Ok(meta) = fs::metadata(&bin_path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if let Ok(guard) = CACHE.lock()
        && let Some(c) = guard.as_ref()
        && c.path == bin_path
        && c.dev == meta.dev()
        && c.ino == meta.ino()
        && c.len == meta.len()
        && c.mtime == meta.mtime()
        && c.mtime_nsec == meta.mtime_nsec()
    {
        return c.verified;
    }
    let verified = bundled_forgejo::is_installed(data_root);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CachedDigest {
            path: bin_path,
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            verified,
        });
    }
    verified
}

fn collect_view_blocking_reasons(
    layout: &Layout,
    view: &BoardView,
    allow_idle: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if !view.terms_acknowledged {
        reasons.push("first-run terms are not acknowledged".to_owned());
    }
    for agent in &view.agents {
        if engine::removal_requested(layout, &agent.name) {
            reasons.push(format!("agent `{}` is queued for removal", agent.name));
        }
        if (allow_idle || agent.desired == Desired::Started)
            && let Some(leftover) = &agent.leftover
        {
            reasons.push(format!(
                "agent `{name}` leftover process: {leftover}; retry cleanup with --control --start-agent {name}",
                name = agent.name
            ));
        }
    }
    let started: Vec<_> = view
        .agents
        .iter()
        .filter(|a| a.desired == Desired::Started)
        .collect();
    if started.is_empty() {
        if view.agents.is_empty() {
            reasons.push("no agents are configured".to_owned());
        } else if !allow_idle {
            reasons.push("no agent is marked started".to_owned());
        }
    }
    for agent in started {
        if let Some(err) = &agent.program_error {
            reasons.push(format!("agent `{}`: {err}", agent.name));
        }
        if agent.instruction_chars == 0 {
            reasons.push(format!("agent `{}` has an empty instruction", agent.name));
        }
        if matches!(agent.session, SessionState::Blocked { .. }) {
            reasons.push(format!("agent `{}` session is blocked", agent.name));
        }
    }
    reasons
}

/// Read-only status inspection (`--status [--json]`). Never acquires `engine.lock`,
/// blocks, or mutates files on disk.
pub fn run_status(cli: StatusCli) -> Result<String, String> {
    let (layout, data_root) = resolve_paths(cli.root, cli.state_root, cli.data_root)?;
    let control_socket_path =
        resolve_control_socket_path(&layout, cli.control_socket.as_deref())?;
    let running = engine_lock_held(&data_root);

    // When a headless daemon holds `engine.lock` and wrote a fresh snapshot for a live
    // PID, return its live runtime view directly.
    if running {
        let snap_path = layout.state_root().join(STATUS_SNAPSHOT_FILE);
        if let Ok(text) = fs::read_to_string(&snap_path)
            && let Ok(mut snap) = serde_json::from_str::<StatusReport>(&text)
            && snap.daemon_pid.is_some_and(process_alive)
        {
            snap.engine_running = true;
            if snap.control_socket.is_empty() {
                snap.control_socket = control_socket_path.display().to_string();
            }
            return format_status(&snap, cli.json);
        }
    }

    let settings_path = layout.root().join(settings::FILE);
    let settings = Settings::load(&settings_path)?;
    let credentials_path = layout.credentials_file();
    let default_key_present = credentials::model_key(&credentials_path, "").is_ok();
    let terms_ok = settings.terms_acknowledged && settings.terms_version >= TERMS_VERSION;

    let workspaces = confinement::agent_workspaces(&layout.agents_dir());
    let method = confinement::detect(
        layout.root(),
        layout.state_root(),
        &data_root,
        &workspaces,
    );

    let instance = InstancePaths::under(&data_root);
    let port = fs::read_to_string(&instance.app_ini)
        .ok()
        .and_then(|ini| bundled_forgejo::port_from_config(&ini));
    let url = port.map(InstancePaths::url).unwrap_or_default();
    let forgejo_reachable = running && !url.is_empty() && probe_forgejo_http(&url);
    let forgejo_state = if forgejo_reachable {
        "running".to_owned()
    } else if running {
        "starting-or-owned-by-active-instance".to_owned()
    } else {
        "stopped".to_owned()
    };

    let mut blocking = Vec::new();
    let mut idle_blocking = Vec::new();
    if !terms_ok {
        blocking.push("first-run terms are not acknowledged".to_owned());
        idle_blocking.push("first-run terms are not acknowledged".to_owned());
    }

    let mut agents = Vec::new();
    let mut started_count = 0usize;
    for saved in &settings.agents {
        let desired = saved.desired();
        if desired == Desired::Started {
            started_count += 1;
        }
        let program = saved.harness();
        let program_installed = if program == Program::Builtin {
            true
        } else {
            harness::installed(&data_root, program).is_some()
        };
        let (protocol, base_url, model) = match saved.provider() {
            Some(p) if program.is_native() => {
                (p.protocol().id().to_owned(), p.base_url.clone(), p.model.clone())
            }
            _ => (
                settings.endpoint.protocol().id().to_owned(),
                settings.endpoint.base_url(),
                settings.endpoint.model.clone(),
            ),
        };
        let key_present = credentials::model_key(&credentials_path, &saved.name).is_ok();
        let instruction_chars = layout
            .agent(&saved.name)
            .ok()
            .and_then(|paths| fs::read_to_string(paths.instruction()).ok())
            .map(|text| text.chars().count())
            .unwrap_or(0);
        let runs_total = count_agent_runs(layout.state_root(), &saved.name);

        let mut prog_err: Option<String> = None;
        let removal_queued = engine::removal_requested(&layout, &saved.name);
        if removal_queued {
            prog_err = Some("queued for removal (awaiting Forgejo token revocation)".to_owned());
            let reason = format!("agent `{}` is queued for removal", saved.name);
            blocking.push(reason.clone());
            idle_blocking.push(reason);
        } else if let Some(unknown) = saved.unknown_harness() {
            prog_err = Some(format!("unknown saved program `{unknown}`"));
        } else if !program_installed {
            prog_err = Some(format!("{} is not installed", program.label()));
        } else if base_url.trim().is_empty() || model.trim().is_empty() {
            prog_err = Some("needs a model address and model id".to_owned());
        } else if let Some(compat) =
            Protocol::parse(&protocol).and_then(|p| harness::compatibility_error(program, p))
        {
            prog_err = Some(compat);
        } else if !key_present {
            prog_err = Some("needs a model key".to_owned());
        } else if program == Program::Dsh
            && let Some(missing) = program.prerequisite_missing()
        {
            prog_err = Some(format!("missing prerequisite: {missing:?}"));
        }

        if desired == Desired::Started {
            if let Some(err) = &prog_err {
                let reason = format!("agent `{}`: {err}", saved.name);
                blocking.push(reason.clone());
                idle_blocking.push(reason);
            }
            if instruction_chars == 0 {
                let reason = format!("agent `{}` has an empty instruction", saved.name);
                blocking.push(reason.clone());
                idle_blocking.push(reason);
            }
        }

        agents.push(AgentStatusSummary {
            name: saved.name.clone(),
            login: format!("gewu-{}", saved.name),
            desired: if removal_queued {
                "removing".to_owned()
            } else {
                match desired {
                    Desired::Started => "started".to_owned(),
                    Desired::Stopped => "stopped".to_owned(),
                }
            },
            stop_on_no_work: false,
            program: program.id().to_owned(),
            program_installed,
            protocol,
            base_url,
            model,
            key_present,
            instruction_chars,
            provisioning: if forgejo_reachable {
                "active".to_owned()
            } else {
                "offline".to_owned()
            },
            session: if running && desired == Desired::Started {
                "managed-by-running-instance".to_owned()
            } else {
                "idle".to_owned()
            },
            runs_total,
            last_result: None,
            last_error: None,
            program_error: prog_err,
        });
    }

    if settings.agents.is_empty() {
        blocking.push("no agents are configured".to_owned());
        idle_blocking.push("no agents are configured".to_owned());
    } else if started_count == 0 {
        blocking.push("no agent is marked started".to_owned());
    }

    let admin_login = if settings.forgejo.admin_login.trim().is_empty() {
        bundled_forgejo::FALLBACK_ADMIN_LOGIN.to_owned()
    } else {
        settings.forgejo.admin_login.clone()
    };

    let report = StatusReport {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        updated_unix_ms: now_unix_ms(),
        engine_running: running,
        daemon_pid: None,
        ready_for_headless: blocking.is_empty(),
        ready_for_idle_headless: idle_blocking.is_empty(),
        healthy: running && blocking.is_empty() && forgejo_reachable,
        idle_standby: running && started_count == 0 && !settings.agents.is_empty(),
        stop_on_no_work: false,
        control_socket: control_socket_path.display().to_string(),
        terms_acknowledged: terms_ok,
        terms_version: settings.terms_version,
        required_terms_version: TERMS_VERSION,
        confined: method.confined(),
        confinement: if method.confined() {
            method.label().to_owned()
        } else {
            "none".to_owned()
        },
        config_dir: layout.root().display().to_string(),
        state_dir: layout.state_root().display().to_string(),
        data_dir: data_root.display().to_string(),
        forgejo: ForgejoStatusSummary {
            binary_installed: bundled_forgejo::is_installed(&data_root),
            version: bundled_forgejo::VERSION.to_owned(),
            state: forgejo_state,
            url,
            admin_login,
            repositories: 0,
        },
        default_endpoint: EndpointStatusSummary {
            configured: settings.endpoint.configured(),
            protocol: settings.endpoint.protocol().id().to_owned(),
            base_url: settings.endpoint.base_url(),
            model: settings.endpoint.model.clone(),
            key_present: default_key_present,
        },
        agents,
        blocking_reasons: blocking,
    };

    format_status(&report, cli.json)
}

fn count_agent_runs(state_root: &Path, agent: &str) -> u64 {
    let Ok(entries) = fs::read_dir(state_root.join(agent)) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|ft| ft.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(run::looks_like_run_id)
        })
        .count() as u64
}

fn probe_forgejo_http(url: &str) -> bool {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    agent
        .get(format!("{url}/api/v1/version"))
        .call()
        .is_ok_and(|resp| (200..=299).contains(&resp.status().as_u16()))
}

fn format_status(report: &StatusReport, as_json: bool) -> Result<String, String> {
    if as_json {
        return serde_json::to_string_pretty(report)
            .map_err(|error| format!("status JSON serialization failed: {error}"));
    }
    let mut lines = Vec::new();
    lines.push(format!(
        "gewu-multiverse {} — engine: {} · ready_for_headless: {} · ready_for_idle_headless: {} · healthy: {} · idle_standby: {}",
        report.version,
        if report.engine_running {
            match report.daemon_pid {
                Some(pid) => format!("running (pid {pid})"),
                None => "running".to_owned(),
            }
        } else {
            "stopped".to_owned()
        },
        report.ready_for_headless,
        report.ready_for_idle_headless,
        report.healthy,
        report.idle_standby
    ));
    lines.push(format!(
        "terms: {} (v{}/{}) · confinement: {} · control_socket: {}",
        if report.terms_acknowledged {
            "acknowledged"
        } else {
            "not acknowledged"
        },
        report.terms_version,
        report.required_terms_version,
        report.confinement,
        report.control_socket
    ));
    lines.push(format!(
        "forgejo {}: binary={} · state={} · url={}",
        report.forgejo.version,
        if report.forgejo.binary_installed {
            "installed"
        } else {
            "missing"
        },
        report.forgejo.state,
        if report.forgejo.url.is_empty() {
            "(unassigned)"
        } else {
            &report.forgejo.url
        }
    ));
    if report.agents.is_empty() {
        lines.push("agents: (none configured)".to_owned());
    } else {
        for agent in &report.agents {
            lines.push(format!(
                "agent {}: desired={}{} · program={} · session={} · runs={} · instruction={} chars{}",
                agent.name,
                agent.desired,
                if agent.stop_on_no_work {
                    " (stop-on-no-work)"
                } else {
                    ""
                },
                agent.program,
                agent.session,
                agent.runs_total,
                agent.instruction_chars,
                agent
                    .program_error
                    .as_ref()
                    .map(|e| format!(" · blocked: {e}"))
                    .unwrap_or_default()
            ));
        }
    }
    if !report.blocking_reasons.is_empty() {
        lines.push(format!("blocking: {}", report.blocking_reasons.join("; ")));
    }
    Ok(lines.join("\n"))
}
