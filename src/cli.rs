//! The arguments the binary accepts.
//!
//! Without a flag the binary opens the board on the real engine. `--demo` opens the
//! same board on the scripted engine in [`crate::board::fake`], for captures.
//! `--headless` runs the real engine without a terminal UI, `--configure` applies
//! non-interactive setup changes, `--control` starts or stops agents on a running
//! headless daemon over its local Unix socket, and `--status` reports read-only state
//! without acquiring the daemon lock.
//! `--install-forgejo` fetches the pinned Forgejo and exits; the first start does the
//! same install by itself when the binary is missing.
//!
//! `--run-agent` and `--git-askpass` are not the board: they are the one-run process
//! and the credential answer the existing product uses. The engine starts its
//! sessions with the first, and the person's own `git` asks the second for an agent's
//! token.

use std::path::PathBuf;

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Open the board. `demo` selects the scripted engine.
    Run(Cli),
    /// Run the real engine as a headless daemon without a terminal UI.
    Headless(HeadlessCli),
    /// Apply non-interactive setup and agent configuration changes, then exit.
    Configure(Box<ConfigureCli>),
    /// Adjust started/stopped agents on a running headless daemon over its local Unix socket.
    Control(Box<ControlCli>),
    /// Print read-only status for operators and service health checks, then exit.
    Status(StatusCli),
    /// Run one agent run and exit, without a terminal.
    RunAgent {
        /// The run directory holding the plan to execute.
        run_dir: PathBuf,
    },
    /// Answer git's credential question and exit. Started by `git`, never by a person.
    GitAskpass {
        /// The prompt git asked with. It is not a secret and is not printed.
        prompt: String,
    },
    /// Become the confined shell of one agent run. Started by a run, never by a
    /// person; the mechanism, command, workspace and hidden paths come from the
    /// environment so no path is ever a command-line word a tool could read back.
    ConfinedShell,
    /// Prove in this child that a Landlock ruleset is fully enforced. Started by the
    /// product's own probe, never by a person.
    LandlockProbe {
        /// The probe root holding the readable control file and the denied secret.
        root: PathBuf,
    },
    /// Apply the product Landlock boundary then exec a native program. Started by a
    /// run, never by a person.
    ConfinedProgram {
        /// The private launch plan.
        spec: PathBuf,
    },
    /// Print one run's model key for a native program's command-backed credential
    /// helper. Started by the native program, never by a person.
    PrintNativeKey {
        /// The run-private key file.
        key: PathBuf,
    },
    /// Serve one run's six Forgejo tools over stdio MCP. Started by the native
    /// program, never by a person.
    McpForgejo {
        /// The run-private context directory.
        context: PathBuf,
    },
    /// The supervisor-owned MCP server. Started by a run supervisor.
    McpForgejoServe {
        /// The run-private socket.
        socket: PathBuf,
        /// The protected supervisor state directory.
        state: PathBuf,
        /// The run directory.
        run: PathBuf,
        /// The credentials file.
        credentials: PathBuf,
        /// The agent.
        agent: String,
        /// The agent workspace.
        workspace: PathBuf,
    },
    /// The thin MCP relay a native CLI starts.
    McpForgejoConnect {
        /// The run-private socket.
        socket: PathBuf,
    },
    /// Install one official native program into the product's data directory.
    InstallHarness {
        /// Program id.
        program: String,
    },
    /// Fetch the pinned Forgejo into the data directory and exit.
    InstallForgejo,
    /// Print the help text.
    Help,
    /// Print the version.
    Version,
}

/// The options of a run of the board.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
    /// The Forgejo/program data directory, when it was given.
    pub data_root: Option<PathBuf>,
    /// Run the board against the scripted engine.
    pub demo: bool,
}

/// Options for `--headless`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadlessCli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
    /// The Forgejo/program data directory, when it was given.
    pub data_root: Option<PathBuf>,
    /// Explicitly acknowledge the first-run terms before starting.
    pub accept_terms: bool,
    /// Keep bundled Forgejo warm even when zero agents are marked `started`.
    pub allow_idle: bool,
    /// Automatically mark an agent `stopped` once a new run finishes with `NoWork`.
    pub stop_on_no_work: bool,
    /// Override the private Unix control socket path (default `<state-dir>/headless.sock`).
    pub control_socket: Option<PathBuf>,
}

/// Options for `--control` (live commands sent to a running `--headless` daemon).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ControlCli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
    /// The Forgejo/program data directory, when it was given.
    pub data_root: Option<PathBuf>,
    /// Override the private Unix control socket path (default `<state-dir>/headless.sock`).
    pub control_socket: Option<PathBuf>,
    /// Agents to wake/start immediately on the running daemon.
    pub start_agents: Vec<String>,
    /// Agents to stop politely on the running daemon without stopping Forgejo.
    pub stop_agents: Vec<String>,
    /// Wake/start every configured agent on the running daemon.
    pub start_all: bool,
    /// Stop every configured agent on the running daemon while keeping Forgejo warm.
    pub stop_all: bool,
    /// Automatically mark started agents `stopped` once a new run finishes with `NoWork`.
    pub stop_on_no_work: bool,
    /// Clear `--stop-on-no-work` so started agents continue polling on `NO_WORK_DELAY`.
    pub keep_running: bool,
    /// Emit machine-readable JSON on standard output.
    pub json: bool,
}

impl ControlCli {
    /// Whether at least one live daemon control action was requested.
    pub fn has_action(&self) -> bool {
        !self.start_agents.is_empty()
            || !self.stop_agents.is_empty()
            || self.start_all
            || self.stop_all
            || self.stop_on_no_work
            || self.keep_running
    }
}

/// Options for `--configure`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigureCli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
    /// The Forgejo/program data directory, when it was given.
    pub data_root: Option<PathBuf>,
    /// Acknowledge the current first-run terms.
    pub accept_terms: bool,
    /// API base URL (or full `/chat/completions` URL) for default or `--agent` model.
    pub base_url: Option<String>,
    /// Wire protocol (`chat-completions`, `responses`, or `anthropic-messages`).
    pub protocol: Option<String>,
    /// Model identifier.
    pub model: Option<String>,
    /// Read the model API key from a file (never from argv).
    pub api_key_file: Option<PathBuf>,
    /// Read the model API key from standard input.
    pub api_key_stdin: bool,
    /// Ensure `agent-1` .. `agent-N` exist.
    pub agents: Option<usize>,
    /// Add one new agent (`agent-N`).
    pub add_agent: bool,
    /// Remove one named agent.
    pub remove_agent: Option<String>,
    /// Target agent name for per-agent program/model/instruction changes.
    pub agent: Option<String>,
    /// Agent runtime program (`builtin`, `pi`, `codex`, `dsh`, or `opencode`).
    pub program: Option<String>,
    /// Codex sandbox mode (`native` or `full-access`).
    pub codex_sandbox: Option<String>,
    /// Path to markdown file to write as the target agent's `instruction.md`.
    pub instruction_file: Option<PathBuf>,
    /// Agents to mark `started` (verifying preconditions).
    pub start_agents: Vec<String>,
    /// Agents to mark `stopped`.
    pub stop_agents: Vec<String>,
    /// Mark all configured agents `started` (verifying preconditions).
    pub start_all: bool,
    /// Mark all configured agents `stopped`.
    pub stop_all: bool,
    /// Test the configured model connection before exiting.
    pub test_model: bool,
}

impl ConfigureCli {
    /// Whether at least one configuration mutation or check was requested.
    pub fn has_action(&self) -> bool {
        self.accept_terms
            || self.base_url.is_some()
            || self.protocol.is_some()
            || self.model.is_some()
            || self.api_key_file.is_some()
            || self.api_key_stdin
            || self.agents.is_some()
            || self.add_agent
            || self.remove_agent.is_some()
            || self.program.is_some()
            || self.codex_sandbox.is_some()
            || self.instruction_file.is_some()
            || !self.start_agents.is_empty()
            || !self.stop_agents.is_empty()
            || self.start_all
            || self.stop_all
            || self.test_model
    }

    /// Whether any offline-only setup flag (not valid under `--control`) was given.
    pub fn has_setup_only_action(&self) -> bool {
        self.accept_terms
            || self.base_url.is_some()
            || self.protocol.is_some()
            || self.model.is_some()
            || self.api_key_file.is_some()
            || self.api_key_stdin
            || self.agents.is_some()
            || self.add_agent
            || self.remove_agent.is_some()
            || self.agent.is_some()
            || self.program.is_some()
            || self.codex_sandbox.is_some()
            || self.instruction_file.is_some()
            || self.test_model
    }
}

/// Options for `--status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusCli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
    /// The Forgejo/program data directory, when it was given.
    pub data_root: Option<PathBuf>,
    /// Optional custom control socket path to include in status checks.
    pub control_socket: Option<PathBuf>,
    /// Emit machine-readable JSON on standard output.
    pub json: bool,
}

/// The help text.
pub const HELP: &str = "\
gewu-multiverse — one Forgejo, the agents wired to it, few words

Usage:
    gewu-multiverse [--config-dir <dir>] [--state-dir <dir>] [--data-dir <dir>]
    gewu-multiverse --headless [--accept-terms] [--allow-idle] [--stop-on-no-work] [--control-socket <file>] [--config-dir <dir>] [--state-dir <dir>] [--data-dir <dir>]
    gewu-multiverse --configure [setup options...] [--config-dir <dir>] [--state-dir <dir>] [--data-dir <dir>]
    gewu-multiverse --control [control options...] [--json] [--control-socket <file>] [--config-dir <dir>] [--state-dir <dir>] [--data-dir <dir>]
    gewu-multiverse --status [--json] [--config-dir <dir>] [--state-dir <dir>] [--data-dir <dir>]
    gewu-multiverse --demo
    gewu-multiverse --install-forgejo
    gewu-multiverse --install-harness <pi|codex|dsh|opencode>
    gewu-multiverse --run-agent <run-dir>

Options:
    --headless    Run the real engine without a terminal UI (for systemd or
                  unattended servers). Fails fast on missing setup; stops
                  cleanly on SIGINT, SIGTERM or SIGHUP.
                    [--allow-idle] [--stop-on-no-work] [--control-socket <file>]
    --configure   Apply non-interactive setup changes and stop:
                    --accept-terms
                    --base-url <url> --model <id> [--protocol <chat-completions|responses|anthropic-messages>]
                    --api-key-file <file> | --api-key-stdin
                    --agents <count> | --add-agent | --remove-agent <name>
                    [--agent <name>] [--program <builtin|pi|codex|dsh|opencode>]
                    [--codex-sandbox <native|full-access>] [--instruction-file <file>]
                    [--start-agent <name>] [--stop-agent <name>] [--start-all] [--stop-all]
                    [--test-model]
    --control     Start or stop agents on a running --headless daemon without
                  restarting Forgejo:
                    [--start-agent <name>] [--stop-agent <name>] [--start-all] [--stop-all]
                    [--stop-on-no-work | --keep-running] [--control-socket <file>] [--json]
    --status      Print read-only daemon, Forgejo and agent state (--json for JSON).
    --demo        Open the board with the scripted engine, for captures. It
                  does not run an agent or start a real Forgejo.
    --install-forgejo    Fetch or verify the pinned Forgejo now and stop. The
                  first start does this by itself when Forgejo is missing.
    --install-harness <program>  Install a pinned native agent program and stop.
    --config-dir <dir>   Put the configuration under <dir>.
    --state-dir <dir>    Put run records under <dir>.
    --data-dir <dir>     Put bundled Forgejo and installed programs under <dir>.
    --run-agent <dir>    Run the one agent run whose plan is in <dir>, then exit.
    -h, --help    Print this help and stop.
    -V, --version Print the version and stop.

The board reads a BoardView and sends Commands. It never reads the configuration,
starts a process or opens a socket itself. No secret is ever passed as an argument.";

fn is_forbidden_secret_flag(argument: &str) -> bool {
    argument == "--api-key"
        || argument.starts_with("--api-key=")
        || argument == "--model-key"
        || argument.starts_with("--model-key=")
        || argument == "--api_key"
        || argument.starts_with("--api_key=")
        || argument == "--model_key"
        || argument.starts_with("--model_key=")
}

/// Read the arguments, without the program name.
pub fn parse_args<I>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = String>,
{
    let raw_args: Vec<String> = args.into_iter().collect();
    for token in &raw_args {
        if token == "--git-askpass" {
            break;
        }
        if is_forbidden_secret_flag(token) {
            return Err(
                "passing an API key on the command line is refused (other processes can read argv); use --api-key-file <file> or --api-key-stdin"
                    .to_owned(),
            );
        }
    }

    let mut root: Option<PathBuf> = None;
    let mut state_root: Option<PathBuf> = None;
    let mut data_root: Option<PathBuf> = None;
    let mut control_socket: Option<PathBuf> = None;
    let mut run_agent: Option<PathBuf> = None;
    let mut demo = false;
    let mut install_forgejo = false;
    let mut headless = false;
    let mut configure = false;
    let mut control = false;
    let mut status = false;
    let mut status_json = false;
    let mut allow_idle = false;
    let mut stop_on_no_work = false;
    let mut keep_running = false;

    let mut cfg = ConfigureCli::default();
    let mut args = raw_args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "--demo" => demo = true,
            "--install-forgejo" => install_forgejo = true,
            "--headless" => headless = true,
            "--configure" => configure = true,
            "--control" => control = true,
            "--status" => status = true,
            "--json" => status_json = true,
            "--allow-idle" => allow_idle = true,
            "--stop-on-no-work" => stop_on_no_work = true,
            "--keep-running" => keep_running = true,
            "--accept-terms" => cfg.accept_terms = true,
            "--api-key-stdin" => cfg.api_key_stdin = true,
            "--add-agent" => cfg.add_agent = true,
            "--start-all" => cfg.start_all = true,
            "--stop-all" => cfg.stop_all = true,
            "--test-model" => cfg.test_model = true,
            // `git` starts this program for a credential, passing the prompt as one
            // argument. This mode never prints help or an error: both would be noise
            // on git's side of the pipe.
            "--git-askpass" => {
                let prompt = args.collect::<Vec<String>>().join(" ");
                return Ok(Invocation::GitAskpass { prompt });
            }
            // A run starts this mode with no other argument. It is not a person's
            // mode and is not in the help.
            "--confined-program" => {
                return Ok(Invocation::ConfinedProgram {
                    spec: value_after(&mut args, "--confined-program")?,
                });
            }
            "--print-native-key" => {
                return Ok(Invocation::PrintNativeKey {
                    key: value_after(&mut args, "--print-native-key")?,
                });
            }
            "--mcp-forgejo" => {
                return Ok(Invocation::McpForgejo {
                    context: value_after(&mut args, "--mcp-forgejo")?,
                });
            }
            "--mcp-forgejo-connect" => {
                return Ok(Invocation::McpForgejoConnect {
                    socket: value_after(&mut args, "--mcp-forgejo-connect")?,
                });
            }
            "--mcp-forgejo-serve" => {
                return Ok(Invocation::McpForgejoServe {
                    socket: value_after(&mut args, "--mcp-forgejo-serve")?,
                    state: value_after(&mut args, "--mcp-forgejo-serve")?,
                    run: value_after(&mut args, "--mcp-forgejo-serve")?,
                    credentials: value_after(&mut args, "--mcp-forgejo-serve")?,
                    agent: value_after(&mut args, "--mcp-forgejo-serve")?
                        .to_string_lossy()
                        .into_owned(),
                    workspace: value_after(&mut args, "--mcp-forgejo-serve")?,
                });
            }
            "--install-harness" => {
                return Ok(Invocation::InstallHarness {
                    program: string_after(&mut args, "--install-harness")?,
                });
            }
            "--confined-shell" => {
                if args.next().is_some() {
                    return Err("--confined-shell takes no argument".to_owned());
                }
                return Ok(Invocation::ConfinedShell);
            }
            // The product's own enforcement probe; it is not a person's mode and is
            // not in the help. The root holds two files and no secret.
            "--landlock-probe" => {
                return Ok(Invocation::LandlockProbe {
                    root: value_after(&mut args, "--landlock-probe")?,
                });
            }
            "--config-dir" => {
                if root.is_some() {
                    return Err("--config-dir was given twice".to_owned());
                }
                root = Some(value_after(&mut args, "--config-dir")?);
            }
            "--state-dir" => {
                if state_root.is_some() {
                    return Err("--state-dir was given twice".to_owned());
                }
                state_root = Some(value_after(&mut args, "--state-dir")?);
            }
            "--data-dir" => {
                if data_root.is_some() {
                    return Err("--data-dir was given twice".to_owned());
                }
                data_root = Some(value_after(&mut args, "--data-dir")?);
            }
            "--control-socket" => {
                if control_socket.is_some() {
                    return Err("--control-socket was given twice".to_owned());
                }
                control_socket = Some(file_after(&mut args, "--control-socket")?);
            }
            "--run-agent" => {
                if run_agent.is_some() {
                    return Err("--run-agent was given twice".to_owned());
                }
                run_agent = Some(value_after(&mut args, "--run-agent")?);
            }
            "--base-url" => {
                if cfg.base_url.is_some() {
                    return Err("--base-url was given twice".to_owned());
                }
                cfg.base_url = Some(string_after(&mut args, "--base-url")?);
            }
            "--protocol" => {
                if cfg.protocol.is_some() {
                    return Err("--protocol was given twice".to_owned());
                }
                cfg.protocol = Some(string_after(&mut args, "--protocol")?);
            }
            "--model" => {
                if cfg.model.is_some() {
                    return Err("--model was given twice".to_owned());
                }
                cfg.model = Some(string_after(&mut args, "--model")?);
            }
            "--api-key-file" => {
                if cfg.api_key_file.is_some() {
                    return Err("--api-key-file was given twice".to_owned());
                }
                cfg.api_key_file = Some(file_after(&mut args, "--api-key-file")?);
            }
            "--agents" => {
                if cfg.agents.is_some() {
                    return Err("--agents was given twice".to_owned());
                }
                let raw = string_after(&mut args, "--agents")?;
                let count = raw
                    .parse::<usize>()
                    .map_err(|_| format!("--agents needs a positive count (got `{raw}`)"))?;
                cfg.agents = Some(count);
            }
            "--remove-agent" => {
                if cfg.remove_agent.is_some() {
                    return Err("--remove-agent was given twice".to_owned());
                }
                cfg.remove_agent = Some(string_after(&mut args, "--remove-agent")?);
            }
            "--agent" => {
                if cfg.agent.is_some() {
                    return Err("--agent was given twice".to_owned());
                }
                cfg.agent = Some(string_after(&mut args, "--agent")?);
            }
            "--program" => {
                if cfg.program.is_some() {
                    return Err("--program was given twice".to_owned());
                }
                cfg.program = Some(string_after(&mut args, "--program")?);
            }
            "--codex-sandbox" => {
                if cfg.codex_sandbox.is_some() {
                    return Err("--codex-sandbox was given twice".to_owned());
                }
                cfg.codex_sandbox = Some(string_after(&mut args, "--codex-sandbox")?);
            }
            "--instruction-file" => {
                if cfg.instruction_file.is_some() {
                    return Err("--instruction-file was given twice".to_owned());
                }
                cfg.instruction_file = Some(file_after(&mut args, "--instruction-file")?);
            }
            "--start-agent" => {
                cfg.start_agents.push(string_after(&mut args, "--start-agent")?);
            }
            "--stop-agent" => {
                cfg.stop_agents.push(string_after(&mut args, "--stop-agent")?);
            }
            other => {
                if other.starts_with('-') {
                    let flag_only = other.split_once('=').map(|(flag, _)| flag).unwrap_or(other);
                    return Err(format!(
                        "`{flag_only}` is not accepted: the arguments are --headless, --configure, --control, --status, --demo, --install-forgejo, --config-dir <dir>, --state-dir <dir>, --data-dir <dir>, --run-agent <run-dir>, --help and --version"
                    ));
                }
                return Err(
                    "unexpected positional argument (value omitted; secrets are never accepted on the command line — use --api-key-file <file> or --api-key-stdin): the arguments are --headless, --configure, --control, --status, --demo, --install-forgejo, --config-dir <dir>, --state-dir <dir>, --data-dir <dir>, --run-agent <run-dir>, --help and --version"
                        .to_owned(),
                );
            }
        }
    }

    if install_forgejo {
        if root.is_some()
            || state_root.is_some()
            || data_root.is_some()
            || control_socket.is_some()
            || demo
            || headless
            || configure
            || control
            || status
            || status_json
            || allow_idle
            || stop_on_no_work
            || keep_running
            || run_agent.is_some()
            || cfg.has_action()
            || cfg.agent.is_some()
        {
            return Err(
                "--install-forgejo works on the default data directory on its own, so it cannot be combined with --demo, --config-dir, --state-dir or --run-agent"
                    .to_owned(),
            );
        }
        return Ok(Invocation::InstallForgejo);
    }

    if let Some(run_dir) = run_agent {
        if root.is_some()
            || state_root.is_some()
            || data_root.is_some()
            || control_socket.is_some()
            || demo
            || headless
            || configure
            || control
            || status
            || status_json
            || allow_idle
            || stop_on_no_work
            || keep_running
            || cfg.has_action()
            || cfg.agent.is_some()
        {
            return Err(
                "--run-agent runs one run on its own, so it cannot be combined with --demo, --config-dir or --state-dir"
                    .to_owned(),
            );
        }
        return Ok(Invocation::RunAgent { run_dir });
    }

    let modes = usize::from(demo)
        + usize::from(headless)
        + usize::from(configure)
        + usize::from(control)
        + usize::from(status);
    if modes > 1 {
        return Err(
            "choose one mode at a time: --headless, --configure, --control, --status or --demo"
                .to_owned(),
        );
    }

    if status_json && !status && !control {
        return Err("--json is only used with --status or --control".to_owned());
    }
    if allow_idle && !headless {
        return Err("--allow-idle is only used with --headless".to_owned());
    }
    if stop_on_no_work && !headless && !control {
        return Err("--stop-on-no-work is only used with --headless or --control".to_owned());
    }
    if keep_running && !control {
        return Err("--keep-running is only used with --control".to_owned());
    }
    if stop_on_no_work && keep_running {
        return Err("pass either --stop-on-no-work or --keep-running, not both".to_owned());
    }
    if control_socket.is_some() && !headless && !control && !status {
        return Err(
            "--control-socket is only used with --headless, --control or --status".to_owned(),
        );
    }
    if cfg.start_all && cfg.stop_all {
        return Err("pass either --start-all or --stop-all, not both".to_owned());
    }
    for agent_name in &cfg.start_agents {
        if cfg.stop_agents.contains(agent_name) {
            return Err(format!(
                "agent `{agent_name}` cannot be both started and stopped in the same command"
            ));
        }
    }

    if status {
        if cfg.has_action() || cfg.agent.is_some() {
            return Err(
                "--status is read-only and cannot be combined with --configure or --control options"
                    .to_owned(),
            );
        }
        return Ok(Invocation::Status(StatusCli {
            root,
            state_root,
            data_root,
            control_socket,
            json: status_json,
        }));
    }

    if headless {
        let mut rest = cfg.clone();
        rest.accept_terms = false;
        if rest.has_action() || rest.agent.is_some() {
            return Err(
                "pass setup options to `gewu-multiverse --configure ...` first, then start `gewu-multiverse --headless` (only --accept-terms, --allow-idle, --stop-on-no-work and --control-socket may be combined directly with --headless)"
                    .to_owned(),
            );
        }
        return Ok(Invocation::Headless(HeadlessCli {
            root,
            state_root,
            data_root,
            accept_terms: cfg.accept_terms,
            allow_idle,
            stop_on_no_work,
            control_socket,
        }));
    }

    if control {
        if cfg.has_setup_only_action() {
            return Err(
                "--control only adjusts running agents (--start-agent, --stop-agent, --start-all, --stop-all, --stop-on-no-work, --keep-running); use --configure while the daemon is stopped for setup changes"
                    .to_owned(),
            );
        }
        let ctrl = ControlCli {
            root,
            state_root,
            data_root,
            control_socket,
            start_agents: cfg.start_agents,
            stop_agents: cfg.stop_agents,
            start_all: cfg.start_all,
            stop_all: cfg.stop_all,
            stop_on_no_work,
            keep_running,
            json: status_json,
        };
        if !ctrl.has_action() {
            return Err(
                "--control needs at least one action (for example --start-agent <name>, --stop-agent <name>, --start-all, --stop-all, --stop-on-no-work or --keep-running)"
                    .to_owned(),
            );
        }
        return Ok(Invocation::Control(Box::new(ctrl)));
    }

    if configure {
        if cfg.api_key_file.is_some() && cfg.api_key_stdin {
            return Err("pass either --api-key-file or --api-key-stdin, not both".to_owned());
        }
        cfg.root = root;
        cfg.state_root = state_root;
        cfg.data_root = data_root;
        if !cfg.has_action() {
            return Err(
                "--configure needs at least one action (for example --accept-terms, --base-url/--model, --agents, --instruction-file or --start-agent)"
                    .to_owned(),
            );
        }
        return Ok(Invocation::Configure(Box::new(cfg)));
    }

    if cfg.has_action() || cfg.agent.is_some() {
        return Err(
            "setup and control flags require `--configure` or `--control` (or `--headless --accept-terms`)"
                .to_owned(),
        );
    }

    Ok(Invocation::Run(Cli {
        root,
        state_root,
        data_root,
        demo,
    }))
}

/// The directory value that must follow an option.
fn value_after(args: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{option} needs a directory after it"))?;
    if value.trim().is_empty() {
        return Err(format!("{option} was given an empty directory"));
    }
    Ok(PathBuf::from(value))
}

/// The file path that must follow an option.
fn file_after(args: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{option} needs a file path after it"))?;
    if value.trim().is_empty() {
        return Err(format!("{option} was given an empty file path"));
    }
    Ok(PathBuf::from(value))
}

/// The non-empty string that must follow an option.
fn string_after(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{option} needs a value after it"))?;
    if value.trim().is_empty() {
        return Err(format!("{option} was given an empty value"));
    }
    Ok(value)
}

/// The help text, with one honest closing line naming the binary and how to remove
/// it.
pub fn help_text() -> String {
    let closing = match std::env::current_exe() {
        Ok(path) => format!(
            "This program is one file — {} — and removing it is the whole uninstall.",
            path.display()
        ),
        Err(_) => "The installer puts it at ~/.local/bin/gewu-multiverse by default, \
                    and removing it is the whole uninstall."
            .to_owned(),
    };
    format!("{HELP}\n\n{closing}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        parse_args(args.iter().map(|argument| (*argument).to_owned()))
    }

    #[test]
    fn demo_and_the_real_board_are_both_accepted() {
        assert_eq!(
            parse(&["--demo"]),
            Ok(Invocation::Run(Cli {
                demo: true,
                ..Cli::default()
            }))
        );
        assert_eq!(parse(&[]), Ok(Invocation::Run(Cli::default())));
    }

    #[test]
    fn headless_configure_control_and_status_parse_cleanly_and_refuse_cli_secrets() {
        assert_eq!(
            parse(&[
                "--headless",
                "--accept-terms",
                "--allow-idle",
                "--stop-on-no-work",
                "--control-socket",
                "/tmp/s/headless.sock",
                "--config-dir",
                "/tmp/c",
            ]),
            Ok(Invocation::Headless(HeadlessCli {
                root: Some(PathBuf::from("/tmp/c")),
                state_root: None,
                data_root: None,
                accept_terms: true,
                allow_idle: true,
                stop_on_no_work: true,
                control_socket: Some(PathBuf::from("/tmp/s/headless.sock")),
            }))
        );
        assert_eq!(
            parse(&[
                "--control",
                "--start-agent",
                "agent-1",
                "--stop-on-no-work",
                "--json",
                "--state-dir",
                "/tmp/st",
            ]),
            Ok(Invocation::Control(Box::new(ControlCli {
                root: None,
                state_root: Some(PathBuf::from("/tmp/st")),
                data_root: None,
                control_socket: None,
                start_agents: vec!["agent-1".to_owned()],
                stop_agents: Vec::new(),
                start_all: false,
                stop_all: false,
                stop_on_no_work: true,
                keep_running: false,
                json: true,
            })))
        );
        assert_eq!(
            parse(&["--status", "--json", "--data-dir", "/tmp/d"]),
            Ok(Invocation::Status(StatusCli {
                root: None,
                state_root: None,
                data_root: Some(PathBuf::from("/tmp/d")),
                control_socket: None,
                json: true,
            }))
        );
        assert!(parse(&["--allow-idle"]).is_err());
        assert!(parse(&["--control"]).is_err());
        assert!(parse(&["--control", "--stop-on-no-work", "--keep-running"]).is_err());
        assert!(parse(&["--control", "--start-all", "--stop-all"]).is_err());
        assert!(parse(&["--control", "--start-agent", "agent-1", "--stop-agent", "agent-1"]).is_err());
        assert!(parse(&["--control", "--agents", "1"]).is_err());
        let refused = parse(&["--configure", "--api-key", "sk-secret"]).expect_err("refused");
        assert!(refused.contains("refused"), "{refused}");
        assert!(!refused.contains("sk-secret"), "{refused}");
        let refused_eq = parse(&["--configure", "--api-key=sk-secret"]).expect_err("refused");
        assert!(refused_eq.contains("refused"), "{refused_eq}");
        assert!(!refused_eq.contains("sk-secret"), "{refused_eq}");
        for argv in [
            &["--configure", "--program", "--api-key", "sk-hidden-leak"][..],
            &["--configure", "--program", "--api-key=sk-hidden-leak"][..],
            &["--configure", "--remove-agent", "--model-key=sk-hidden-leak"][..],
            &["--configure", "--secret=sk-hidden-leak"][..],
            &["--configure", "sk-hidden-leak"][..],
            &["--control", "--api-key", "sk-hidden-leak"][..],
            &["--control", "sk-hidden-leak"][..],
        ] {
            let err = parse(argv).expect_err("refused");
            assert!(
                !err.contains("sk-hidden-leak"),
                "secret echoed for {argv:?}: {err}"
            );
        }
    }

    #[test]
    fn the_installer_is_its_own_invocation_and_takes_no_other_option() {
        assert_eq!(
            parse(&["--install-forgejo"]),
            Ok(Invocation::InstallForgejo)
        );
        assert!(parse(&["--install-forgejo", "--demo"]).is_err());
        assert!(parse(&["--install-forgejo", "--config-dir", "/tmp/x"]).is_err());
        assert!(parse(&["--install-forgejo", "--run-agent", "/tmp/r"]).is_err());
    }

    #[test]
    fn help_and_version_stop_before_anything_else() {
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
        assert_eq!(parse(&["-V"]), Ok(Invocation::Version));
    }

    #[test]
    fn an_unknown_argument_is_refused_with_the_list() {
        let message = parse(&["--engine"]).expect_err("refused");
        assert!(message.contains("--demo"), "{message}");
        assert!(message.contains("not accepted"), "{message}");
    }

    #[test]
    fn the_agent_run_mode_still_takes_one_directory_and_nothing_else() {
        assert_eq!(
            parse(&["--run-agent", "/tmp/run-1"]),
            Ok(Invocation::RunAgent {
                run_dir: PathBuf::from("/tmp/run-1")
            })
        );
        assert!(parse(&["--run-agent", "/tmp/run-1", "--config-dir", "/tmp/x"]).is_err());
        assert!(parse(&["--run-agent", "/tmp/run-1", "--demo"]).is_err());
    }

    #[test]
    fn the_board_options_keep_their_old_shape() {
        assert_eq!(
            parse(&[
                "--config-dir",
                "/tmp/example",
                "--state-dir",
                "/tmp/state",
                "--demo"
            ]),
            Ok(Invocation::Run(Cli {
                root: Some(PathBuf::from("/tmp/example")),
                state_root: Some(PathBuf::from("/tmp/state")),
                data_root: None,
                demo: true,
            }))
        );
    }

    #[test]
    fn the_askpass_mode_takes_the_prompt_and_answers_immediately() {
        assert_eq!(
            parse(&["--git-askpass", "Password for 'https://example.org':"]),
            Ok(Invocation::GitAskpass {
                prompt: "Password for 'https://example.org':".to_owned()
            })
        );
    }

    #[test]
    fn the_help_names_the_real_board_the_demo_and_the_installer() {
        assert!(HELP.contains("--demo"));
        assert!(HELP.contains("--headless"));
        assert!(HELP.contains("--allow-idle"));
        assert!(HELP.contains("--control"));
        assert!(HELP.contains("--stop-on-no-work"));
        assert!(HELP.contains("--configure"));
        assert!(HELP.contains("--status"));
        assert!(HELP.contains("--install-forgejo"));
        assert!(!HELP.contains("real engine yet"));
        assert!(HELP.contains("scripted engine"));
    }
}
