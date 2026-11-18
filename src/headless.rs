//! Non-interactive configuration (`--configure`), unattended daemon execution
//! (`--headless`), and lock-free read-only inspection (`--status`).
//!
//! All three modes share the engine's layout, `0600` credentials boundary, Landlock
//! detection and single-instance lock rules without requiring a terminal. Secrets
//! never enter command-line arguments, status output, snapshots or logs.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use signal_hook::SigId;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::bundled_forgejo::{self, InstancePaths};
use crate::cli::{ConfigureCli, HeadlessCli, StatusCli};
use crate::confinement;
use crate::credentials;
use crate::engine::settings::{self, Settings};
use crate::engine::{self, EngineOptions, TERMS_VERSION};
use crate::engine_api::{
    BoardView, Command, Desired, EndpointTest, EngineApi, ForgejoState, Provisioning, SessionEnd,
    SessionState,
};
use crate::harness::{self, Program, Protocol};
use crate::paths::Layout;
use crate::run::{self, process_alive};
use crate::time::now_unix_ms;

/// Private runtime heartbeat/snapshot written by `--headless` under the state root.
pub const STATUS_SNAPSHOT_FILE: &str = "headless-status.json";

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

    for stop_name in &cli.stop_agents {
        if engine::removal_requested(layout, stop_name) {
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

fn validate_headless_preconditions(layout: &Layout, view: &BoardView) -> Result<(), String> {
    if !view.terms_acknowledged {
        return Err(
            "first-run terms have not been acknowledged; run `gewu-multiverse --configure --accept-terms` (or pass `--accept-terms`) first"
                .to_owned(),
        );
    }
    let started: Vec<_> = view
        .agents
        .iter()
        .filter(|agent| agent.desired == Desired::Started)
        .collect();
    if started.is_empty() {
        let queued: Vec<&str> = view
            .agents
            .iter()
            .filter(|a| engine::removal_requested(layout, &a.name))
            .map(|a| a.name.as_str())
            .collect();
        if let Some(first) = queued.first() {
            return Err(format!(
                "no agent is set to start (pending removal for `{}`: run `gewu-multiverse --configure --remove-agent {first}` to finish revoking its Forgejo sign-in, and start at least one agent with `--start-agent <name>`)",
                queued.join(", ")
            ));
        }
        return Err(
            "no agent is set to start; configure and start at least one agent with `gewu-multiverse --configure ... --start-agent <name>`"
                .to_owned(),
        );
    }
    for agent in started {
        if engine::removal_requested(layout, &agent.name) {
            return Err(format!(
                "agent `{}` is queued for removal; finish removing it with `gewu-multiverse --configure --remove-agent {}`",
                agent.name, agent.name
            ));
        }
        if let Some(leftover) = &agent.leftover {
            return Err(format!(
                "agent `{}` has an unfinished earlier process group ({leftover}) — stop the leftover process group or retry cleanup with `gewu-multiverse --configure --start-agent {}`",
                agent.name, agent.name
            ));
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

        validate_headless_preconditions(&layout, &view)?;

        let started_names: Vec<String> = view
            .agents
            .iter()
            .filter(|a| a.desired == Desired::Started)
            .map(|a| a.name.clone())
            .collect();
        let confine_label = view
            .confinement
            .clone()
            .unwrap_or_else(|| "none (runs are not confined on this computer)".to_owned());
        log_daemon(&format!(
            "starting headless daemon (pid {}, confinement: {confine_label}, started agents: {})",
            std::process::id(),
            started_names.join(", ")
        ));

        let mut forgejo_ever_ran = matches!(view.forgejo.state, ForgejoState::Running);
        let mut last_forgejo_line = String::new();
        let mut last_agent_lines: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_snapshot_json = String::new();

        loop {
            if signals.requested() {
                log_daemon("termination signal received; shutting down");
                break;
            }
            engine.tick();
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
                    continue;
                }
                let line = describe_agent_runtime(agent);
                if last_agent_lines.get(&agent.name) != Some(&line) {
                    log_daemon(&format!("agent `{}`: {line}", agent.name));
                    last_agent_lines.insert(agent.name.clone(), line);
                }
            }

            let snap = build_snapshot_from_view(&layout, &data_root, &view, true);
            if let Ok(json) = serde_json::to_string_pretty(&snap)
                && json != last_snapshot_json
            {
                let _ = write_snapshot_file(&state_root, &json);
                last_snapshot_json = json;
            }

            for _ in 0..5 {
                if signals.requested() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    })();

    engine.shutdown(stop_grace);
    let final_view = engine.board();
    let final_snap = build_snapshot_from_view(&layout, &data_root, &final_view, false);
    if let Ok(json) = serde_json::to_string_pretty(&final_snap) {
        let _ = write_snapshot_file(&state_root, &json);
    }
    if outcome.is_ok() {
        log_daemon("stopped every started agent and Forgejo cleanly");
    }
    outcome
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

/// Non-secret status report emitted by `--status` and `headless-status.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub version: String,
    pub updated_unix_ms: u64,
    pub engine_running: bool,
    pub daemon_pid: Option<u32>,
    pub ready_for_headless: bool,
    pub healthy: bool,
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

fn build_snapshot_from_view(
    layout: &Layout,
    data_root: &Path,
    view: &BoardView,
    running: bool,
) -> StatusReport {
    let blocking = collect_view_blocking_reasons(view);
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
    let healthy = running
        && blocking.is_empty()
        && matches!(view.forgejo.state, ForgejoState::Running);
    StatusReport {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        updated_unix_ms: now_unix_ms(),
        engine_running: running,
        daemon_pid: running.then(std::process::id),
        ready_for_headless: blocking.is_empty(),
        healthy,
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
            binary_installed: bundled_forgejo::is_installed(data_root),
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

fn collect_view_blocking_reasons(view: &BoardView) -> Vec<String> {
    let mut reasons = Vec::new();
    if !view.terms_acknowledged {
        reasons.push("first-run terms are not acknowledged".to_owned());
    }
    let started: Vec<_> = view
        .agents
        .iter()
        .filter(|a| a.desired == Desired::Started)
        .collect();
    if started.is_empty() {
        reasons.push("no agent is marked started".to_owned());
    }
    for agent in started {
        if let Some(leftover) = &agent.leftover {
            reasons.push(format!("agent `{}` leftover process: {leftover}", agent.name));
        }
        if let Some(err) = &agent.program_error {
            reasons.push(format!("agent `{}`: {err}", agent.name));
        }
        if agent.instruction_chars == 0 {
            reasons.push(format!("agent `{}` has an empty instruction", agent.name));
        }
    }
    reasons
}

/// Read-only status inspection (`--status [--json]`). Never acquires `engine.lock`
/// blocks or mutates files on disk.
pub fn run_status(cli: StatusCli) -> Result<String, String> {
    let (layout, data_root) = resolve_paths(cli.root, cli.state_root, cli.data_root)?;
    let running = engine_lock_held(&data_root);

    // When a headless daemon holds `engine.lock` and wrote a fresh snapshot for a live
    // PID, return its live runtime view directly (refreshing loopback Forgejo liveness).
    if running {
        let snap_path = layout.state_root().join(STATUS_SNAPSHOT_FILE);
        if let Ok(text) = fs::read_to_string(&snap_path)
            && let Ok(mut snap) = serde_json::from_str::<StatusReport>(&text)
            && snap.daemon_pid.is_some_and(process_alive)
        {
            snap.engine_running = true;
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
    if !terms_ok {
        blocking.push("first-run terms are not acknowledged".to_owned());
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
                blocking.push(format!("agent `{}`: {err}", saved.name));
            }
            if instruction_chars == 0 {
                blocking.push(format!("agent `{}` has an empty instruction", saved.name));
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

    if started_count == 0 {
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
        healthy: running && blocking.is_empty() && forgejo_reachable,
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
        "gewu-multiverse {} — engine: {} · ready_for_headless: {} · healthy: {}",
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
        report.healthy
    ));
    lines.push(format!(
        "terms: {} (v{}/{}) · confinement: {}",
        if report.terms_acknowledged {
            "acknowledged"
        } else {
            "not acknowledged"
        },
        report.terms_version,
        report.required_terms_version,
        report.confinement
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
                "agent {}: desired={} · program={} · session={} · runs={} · instruction={} chars{}",
                agent.name,
                agent.desired,
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
