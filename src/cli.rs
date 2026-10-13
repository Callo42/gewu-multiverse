//! The arguments the program accepts.
//!
//! There are three, and no more: `--config-dir <dir>` to move the one directory the
//! files live in, `--help`, and `--version`. Anything else is refused, and the
//! message says why: a secret is never passed as an argument, because arguments are
//! visible to every process on the machine. Secrets are read from the credentials
//! file, which is mode 0600.

use std::path::PathBuf;

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Start the terminal UI.
    Run(Cli),
    /// Print the help text and stop.
    Help,
    /// Print the version and stop.
    Version,
}

/// The options of a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    /// The directory the three files live in, when it was given.
    pub root: Option<PathBuf>,
}

/// The help text, also shown when an argument is refused.
pub const HELP: &str = "\
gewu-multiverse — a local, single-Universe Matrixverse

Usage:
    gewu-multiverse [--config-dir <dir>]

Options:
    --config-dir <dir>   Put config.toml, credentials.toml and the first-run
                         acknowledgment in <dir> instead of the user's own
                         configuration directory.
    -h, --help           Print this help and stop.
    -V, --version        Print the version and stop.

No secret is ever passed as an argument; put secrets in the credentials file,
which is created with mode 0600.";

/// Read the arguments, without the program name.
pub fn parse_args<I>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = String>,
{
    let mut root: Option<PathBuf> = None;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "--config-dir" => {
                if root.is_some() {
                    return Err("--config-dir was given twice".to_owned());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "--config-dir needs a directory after it".to_owned())?;
                if value.trim().is_empty() {
                    return Err("--config-dir was given an empty directory".to_owned());
                }
                root = Some(PathBuf::from(value));
            }
            other => {
                return Err(format!(
                    "`{other}` is not accepted: the only arguments are --config-dir <dir>, --help and --version, and a secret is never passed as an argument"
                ));
            }
        }
    }
    Ok(Invocation::Run(Cli { root }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_runs_with_the_user_configuration_directory() {
        assert_eq!(parse(&[]), Ok(Invocation::Run(Cli { root: None })));
    }

    #[test]
    fn a_configuration_directory_can_be_chosen() {
        let invocation = parse(&["--config-dir", "/tmp/example"]).expect("accepted");
        assert_eq!(
            invocation,
            Invocation::Run(Cli {
                root: Some(PathBuf::from("/tmp/example"))
            })
        );
    }

    #[test]
    fn help_and_version_stop_before_anything_else_happens() {
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
    }

    #[test]
    fn a_configuration_directory_without_a_value_is_refused() {
        let message = parse(&["--config-dir"]).expect_err("refused");
        assert!(message.contains("needs a directory"), "{message}");
    }

    #[test]
    fn an_empty_configuration_directory_is_refused() {
        assert!(parse(&["--config-dir", "  "]).is_err());
    }

    #[test]
    fn a_configuration_directory_given_twice_is_refused() {
        assert!(parse(&["--config-dir", "/a", "--config-dir", "/b"]).is_err());
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
}
