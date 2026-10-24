//! The arguments the binary accepts.
//!
//! Without a flag the binary opens the board on the real engine. `--demo` opens the
//! same board on the scripted engine in [`crate::board::fake`], for captures.
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
    /// Run the board against the scripted engine.
    pub demo: bool,
}

/// The help text.
pub const HELP: &str = "\
gewu-multiverse — one Forgejo, the agents wired to it, few words

Usage:
    gewu-multiverse
    gewu-multiverse --demo
    gewu-multiverse --install-forgejo
    gewu-multiverse [--config-dir <dir>] [--state-dir <dir>]
    gewu-multiverse --run-agent <run-dir>

Options:
    --demo        Open the board with the scripted engine, for captures. It
                  does not run an agent or start a real Forgejo.
    --install-forgejo    Fetch the pinned Forgejo now and stop. The first start
                  does this by itself when Forgejo is missing.
    --config-dir <dir>   Put the configuration under <dir>.
    --state-dir <dir>    Put run records under <dir>.
    --run-agent <dir>    Run the one agent run whose plan is in <dir>, then exit.
    -h, --help    Print this help and stop.
    -V, --version Print the version and stop.

The board reads a BoardView and sends Commands. It never reads the configuration,
starts a process or opens a socket itself. No secret is ever passed as an argument.";

/// Read the arguments, without the program name.
pub fn parse_args<I>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = String>,
{
    let mut root: Option<PathBuf> = None;
    let mut state_root: Option<PathBuf> = None;
    let mut run_agent: Option<PathBuf> = None;
    let mut demo = false;
    let mut install_forgejo = false;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "--demo" => demo = true,
            "--install-forgejo" => install_forgejo = true,
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
                    program: value_after(&mut args, "--install-harness")?
                        .to_string_lossy()
                        .into_owned(),
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
            "--run-agent" => {
                if run_agent.is_some() {
                    return Err("--run-agent was given twice".to_owned());
                }
                run_agent = Some(value_after(&mut args, "--run-agent")?);
            }
            other => {
                return Err(format!(
                    "`{other}` is not accepted: the arguments are --demo, --install-forgejo, --config-dir <dir>, --state-dir <dir>, --run-agent <run-dir>, --help and --version"
                ));
            }
        }
    }

    if install_forgejo {
        if root.is_some() || state_root.is_some() || demo || run_agent.is_some() {
            return Err(
                "--install-forgejo works on the default data directory on its own, so it cannot be combined with --demo, --config-dir, --state-dir or --run-agent"
                    .to_owned(),
            );
        }
        return Ok(Invocation::InstallForgejo);
    }

    if let Some(run_dir) = run_agent {
        if root.is_some() || state_root.is_some() || demo {
            return Err(
                "--run-agent runs one run on its own, so it cannot be combined with --demo, --config-dir or --state-dir"
                    .to_owned(),
            );
        }
        return Ok(Invocation::RunAgent { run_dir });
    }
    Ok(Invocation::Run(Cli {
        root,
        state_root,
        demo,
    }))
}

/// The value that must follow an option.
fn value_after(args: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{option} needs a directory after it"))?;
    if value.trim().is_empty() {
        return Err(format!("{option} was given an empty directory"));
    }
    Ok(PathBuf::from(value))
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
        assert!(HELP.contains("--install-forgejo"));
        assert!(!HELP.contains("real engine yet"));
        assert!(HELP.contains("scripted engine"));
    }
}
