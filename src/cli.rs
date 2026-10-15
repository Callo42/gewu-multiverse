//! The arguments the program accepts.
//!
//! For a person, there are a few: `--config-dir <dir>` to move the configuration
//! directory, `--state-dir <dir>` to move the run records, `--help` and `--version`.
//! Anything else is refused, and the message says why: a secret is never passed as an
//! argument, because arguments are visible to every process on the machine. Secrets
//! are read from the credentials file, which is mode 0600.
//!
//! One more argument exists for the tool itself: `--run-agent <run dir>` runs one
//! agent run and exits. That is how the terminal UI starts an agent — as a child
//! process of itself — and it is what a person can run by hand to watch a run
//! without the UI. It is documented rather than hidden, and it carries no secret:
//! everything a run needs is in the plan written into the run directory.

use std::path::PathBuf;

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Start the terminal UI.
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
    /// Print the help text and stop.
    Help,
    /// Print the version and stop.
    Version,
}

/// The options of a run of the terminal UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    /// The configuration directory, when it was given.
    pub root: Option<PathBuf>,
    /// The run-record directory, when it was given.
    pub state_root: Option<PathBuf>,
}

/// The help text, also shown when an argument is refused.
pub const HELP: &str = "\
gewu-multiverse — a local, single-Universe Matrixverse

Usage:
    gewu-multiverse [--config-dir <dir>] [--state-dir <dir>]
    gewu-multiverse --run-agent <run-dir>

Options:
    --config-dir <dir>   Put config.toml, credentials.toml, the first-run
                         acknowledgment, the endpoint-test record and each agent's
                         agents/<name>/ directory
                         in <dir> instead of the user's own configuration
                         directory (~/.config/gewu-multiverse).
    --state-dir <dir>    Put run records in <dir> instead of the user's own state
                         directory (~/.local/state/gewu-multiverse).
    --run-agent <dir>    Run the one agent run whose plan is in <dir>, then exit.
                         This is how the terminal UI starts an agent, and it is not
                         a secret: the plan holds paths and shell steps, nothing
                         more.
    -h, --help           Print this help and stop.
    -V, --version        Print the version and stop.

This configuration, its credentials and its run records belong to the machine they
were made on. Copying the configuration directory to a second machine carries the
Forgejo address, the secrets and the endpoint-test record with it; the run records
live under the state directory and do not travel with it. A record that names
another machine is labelled as such where it is shown.

No secret is ever passed as an argument; put secrets in the credentials file,
which is created with mode 0600.";

/// The help text, with one honest closing line naming the binary and how to remove
/// it (F8).
///
/// The install left a single file somewhere; nothing in the product said where it
/// was or that removing it is the whole uninstall, so a person handing the machine
/// back had to remember the installer's line. `current_exe` is the binary actually
/// running, so the sentence is true of this build wherever it was put; the fallback
/// names the installer's own default when the path cannot be read.
pub fn help_text() -> String {
    let closing = match std::env::current_exe() {
        Ok(path) => format!(
            "This program is one file — {} — and removing it is the whole uninstall.\n\
             The configuration and state directories above are left as they are.",
            path.display()
        ),
        Err(_) => "This program is one file (the installer puts it at \
                    ~/.local/bin/gewu-multiverse by default), and removing it is the\n\
                    whole uninstall. The configuration and state directories above are\n\
                    left as they are."
            .to_owned(),
    };
    format!("{HELP}\n\n{closing}")
}

/// Read the arguments, without the program name.
pub fn parse_args<I>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = String>,
{
    let mut root: Option<PathBuf> = None;
    let mut state_root: Option<PathBuf> = None;
    let mut run_agent: Option<PathBuf> = None;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            // `git` starts this program for a credential, passing the prompt as one
            // argument. This mode is answered immediately: it must never print help or
            // an error, because both would be noise on git's side of the pipe.
            "--git-askpass" => {
                let prompt = args.collect::<Vec<String>>().join(" ");
                return Ok(Invocation::GitAskpass { prompt });
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
                    "`{other}` is not accepted: the arguments are --config-dir <dir>, --state-dir <dir>, --run-agent <run-dir>, --help and --version, and a secret is never passed as an argument"
                ));
            }
        }
    }

    if let Some(run_dir) = run_agent {
        if root.is_some() || state_root.is_some() {
            return Err(
                "--run-agent runs one run on its own, so it cannot be combined with --config-dir or --state-dir"
                    .to_owned(),
            );
        }
        return Ok(Invocation::RunAgent { run_dir });
    }
    Ok(Invocation::Run(Cli { root, state_root }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_runs_with_the_user_configuration_directory() {
        assert_eq!(
            parse(&[]),
            Ok(Invocation::Run(Cli {
                root: None,
                state_root: None
            }))
        );
    }

    #[test]
    fn a_configuration_directory_and_a_state_directory_can_be_chosen() {
        let invocation = parse(&["--config-dir", "/tmp/example", "--state-dir", "/tmp/state"])
            .expect("accepted");
        assert_eq!(
            invocation,
            Invocation::Run(Cli {
                root: Some(PathBuf::from("/tmp/example")),
                state_root: Some(PathBuf::from("/tmp/state")),
            })
        );
    }

    #[test]
    fn the_agent_run_mode_takes_one_directory_and_nothing_else() {
        assert_eq!(
            parse(&["--run-agent", "/tmp/run-1"]),
            Ok(Invocation::RunAgent {
                run_dir: PathBuf::from("/tmp/run-1")
            })
        );
        assert!(parse(&["--run-agent", "/tmp/run-1", "--config-dir", "/tmp/x"]).is_err());
        assert!(parse(&["--run-agent", "/tmp/run-1", "--run-agent", "/tmp/run-2"]).is_err());
    }

    #[test]
    fn help_and_version_stop_before_anything_else_happens() {
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
    }

    #[test]
    fn a_directory_without_a_value_is_refused() {
        assert!(parse(&["--config-dir"]).is_err());
        assert!(parse(&["--state-dir"]).is_err());
        assert!(parse(&["--run-agent"]).is_err());
    }

    #[test]
    fn an_empty_directory_is_refused() {
        assert!(parse(&["--config-dir", "  "]).is_err());
        assert!(parse(&["--state-dir", "  "]).is_err());
    }

    #[test]
    fn a_directory_given_twice_is_refused() {
        assert!(parse(&["--config-dir", "/a", "--config-dir", "/b"]).is_err());
        assert!(parse(&["--state-dir", "/a", "--state-dir", "/b"]).is_err());
    }

    #[test]
    fn the_askpass_mode_takes_the_prompt_and_answers_immediately() {
        assert_eq!(
            parse(&["--git-askpass", "Password for 'https://example.org':"]),
            Ok(Invocation::GitAskpass {
                prompt: "Password for 'https://example.org':".to_owned()
            })
        );
        assert_eq!(
            parse(&["--git-askpass"]),
            Ok(Invocation::GitAskpass {
                prompt: String::new()
            })
        );
    }

    #[test]
    fn a_secret_cannot_be_passed_as_an_argument() {
        for argument in ["--token", "token=abcdef", "--token=abcdef", "-t"] {
            let message = parse(&[argument]).expect_err("refused");
            assert!(
                message.contains("not accepted") && message.contains("never passed as an argument"),
                "{message}"
            );
        }
    }

    #[test]
    fn the_help_text_says_where_secrets_go() {
        assert!(HELP.contains("credentials file"));
        assert!(HELP.contains("0600"));
    }

    #[test]
    fn the_help_text_says_the_configuration_belongs_to_one_machine() {
        // W32-6: the product never said it is one machine. A person who copies the
        // configuration to a second machine is told what travels and what does not.
        assert!(HELP.contains("belong to the machine they\nwere made on"), "{HELP}");
        assert!(HELP.contains("run records"), "{HELP}");
        assert!(HELP.contains("do not travel"), "{HELP}");
    }

    #[test]
    fn the_help_text_says_where_the_binary_is_and_how_to_remove_it() {
        // F8: a person handing the machine back had no sentence about the 5.8 MB file
        // the installer left. The closing line names the running binary and the whole
        // uninstall.
        let text = help_text();
        assert!(text.starts_with(HELP), "the option list is unchanged");
        assert!(text.contains("removing it is the whole uninstall"), "{text}");
        assert!(
            text.contains("configuration and state directories above are left"),
            "{text}"
        );
        // The named path is this test binary's own, so the sentence is true of the
        // artifact it is printed from.
        if let Ok(exe) = std::env::current_exe() {
            assert!(text.contains(&exe.display().to_string()), "{text}");
        }
    }
}
