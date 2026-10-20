//! The engine's states in the words a first-time user reads.
//!
//! Every screen takes its state words from here, so a state is renamed in one place
//! and the board, the agent card, the Forgejo box and the panel cannot drift apart.
//! Colour is decided here too; every face also carries a symbol and a word, so a
//! colour-blind reader loses nothing.

use crate::board::theme;
use crate::engine_api::{AgentView, Desired, ForgejoState, Provisioning, SessionEnd, SessionState};
use ratatui::style::Color;
use std::time::SystemTime;

/// The symbol, word, colour and one-line status of a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Face {
    pub symbol: char,
    pub word: String,
    pub colour: Color,
    pub status: String,
}

/// Remove the engine's technical words from anything that may reach the screen.
///
/// The engine contract is allowed to say `endpoint`, `token`, `session` and the rest;
/// the person is not. This is the one translation point for a free-text reason or an
/// activity string, so no such word can leak through a notice, a reason or an error.
pub fn plain(text: &str) -> String {
    text.replace("endpoint", "model")
        .replace("provisioning", "setting up")
        .replace("token", "login")
        .replace("session", "run")
        .replace("missing", "not set up")
        .replace("blocked", "stopped")
        .replace('`', "")
}

/// The blocked reason in the words the card uses.
///
/// Unknown reasons are shortened rather than shown raw: a reason is one short line,
/// and it must not carry an engine word onto the screen.
pub fn blocked_words(reason: &str) -> &'static str {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("instruction") {
        "needs an instruction"
    } else if reason.contains("model") || reason.contains("endpoint") || reason.contains("key") {
        "needs a model"
    } else if reason.contains("account") {
        "account not ready"
    } else if reason.contains("forgejo") {
        "waiting for Forgejo"
    } else {
        "needs help"
    }
}

/// The newest activity, in the words the card uses.
pub fn activity_words(activity: &str) -> String {
    let activity = plain(activity);
    if let Some(rest) = activity.strip_prefix("commented #") {
        return format!("commented on #{rest}");
    }
    if let Some(rest) = activity.strip_prefix("opened PR #") {
        return format!("opened pull request #{rest}");
    }
    if let Some(rest) = activity.strip_prefix("opened pr #") {
        return format!("opened pull request #{rest}");
    }
    if let Some(rest) = activity.strip_prefix("opened #") {
        return format!("opened pull request #{rest}");
    }
    if let Some(rest) = activity.strip_prefix("updated #") {
        return format!("updated pull request #{rest}");
    }
    activity
}

/// The Forgejo box's face.
pub fn forgejo_face(state: &ForgejoState) -> Face {
    match state {
        ForgejoState::Missing => Face {
            symbol: '○',
            word: "not set up yet".to_owned(),
            colour: theme::GREY,
            status: "not set up yet".to_owned(),
        },
        ForgejoState::Fetching { percent } => {
            let status = match percent {
                Some(percent) => format!("downloading {percent}%"),
                None => "downloading".to_owned(),
            };
            Face {
                symbol: '◐',
                word: status.clone(),
                colour: theme::AMBER,
                status,
            }
        }
        ForgejoState::Preparing => Face {
            symbol: '◐',
            word: "setting up".to_owned(),
            colour: theme::AMBER,
            status: "setting up".to_owned(),
        },
        ForgejoState::Starting => Face {
            symbol: '◐',
            word: "starting".to_owned(),
            colour: theme::AMBER,
            status: "starting".to_owned(),
        },
        ForgejoState::Running => Face {
            symbol: '●',
            word: "running".to_owned(),
            colour: theme::RUNNING,
            status: "running".to_owned(),
        },
        ForgejoState::Stopped => Face {
            symbol: '○',
            word: "stopped".to_owned(),
            colour: theme::GREY,
            status: "stopped".to_owned(),
        },
        ForgejoState::Failed { reason } => {
            let status = format!("stopped: {}", plain(reason));
            Face {
                symbol: '●',
                word: status.clone(),
                colour: theme::RED,
                status,
            }
        }
    }
}

/// The agent's face, with the state word and the one status line.
pub fn agent_face(agent: &AgentView) -> Face {
    if agent.provisioning == Provisioning::Pending {
        return Face {
            symbol: '○',
            word: "connecting".to_owned(),
            colour: theme::GREY,
            status: "waiting for Forgejo".to_owned(),
        };
    }
    if let Provisioning::Failed { reason } = &agent.provisioning {
        let _ = reason;
        return Face {
            symbol: '●',
            word: "login failed".to_owned(),
            colour: theme::RED,
            status: "login failed".to_owned(),
        };
    }
    match &agent.session {
        SessionState::Running { .. } => Face {
            symbol: '●',
            word: "working".to_owned(),
            colour: theme::RUNNING,
            status: "working".to_owned(),
        },
        SessionState::Restarting { next_at, .. } => {
            let seconds = next_at
                .duration_since(SystemTime::now())
                .map(|left| left.as_secs().max(1))
                .unwrap_or(1);
            let status = format!("next run in {seconds}s");
            Face {
                symbol: '◐',
                word: status.clone(),
                colour: theme::AMBER,
                status,
            }
        }
        SessionState::Blocked { reason } => {
            let status = blocked_words(reason).to_owned();
            Face {
                symbol: '●',
                word: status.clone(),
                colour: theme::RED,
                status,
            }
        }
        SessionState::Idle => {
            if agent.desired == Desired::Started {
                Face {
                    symbol: '◐',
                    word: "starting".to_owned(),
                    colour: theme::AMBER,
                    status: "starting".to_owned(),
                }
            } else {
                Face {
                    symbol: '○',
                    word: "stopped".to_owned(),
                    colour: theme::GREY,
                    status: "stopped".to_owned(),
                }
            }
        }
    }
}

/// The newest activity of a running agent, in the card's words, if it has one.
pub fn activity(agent: &AgentView) -> Option<String> {
    if !matches!(agent.session, SessionState::Running { .. }) {
        return None;
    }
    agent.activity.as_deref().map(activity_words)
}

/// The run count shown on the agent panel: `run 4`.
pub fn run_count(agent: &AgentView) -> String {
    format!("run {}", agent.sessions_since_start)
}

/// The symbol and colour of one finished run.
pub fn session_symbol(end: &SessionEnd) -> (char, Color) {
    match end {
        SessionEnd::Succeeded => ('✓', theme::RUNNING),
        SessionEnd::Failed { .. } => ('✗', theme::RED),
        SessionEnd::TimedOut => ('◷', theme::AMBER),
        SessionEnd::Stopped => ('■', theme::GREY),
    }
}

/// Whether a word is one the product must not show. Used by the render tests.
pub const BANNED: [&str; 9] = [
    "session",
    "provisioning",
    "token",
    "endpoint",
    "missing",
    "blocked",
    "process group",
    "loopback",
    "2xx",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn agent(session: SessionState, desired: Desired) -> AgentView {
        AgentView {
            name: "agent-1".to_owned(),
            login: "gewu-agent-1".to_owned(),
            provisioning: Provisioning::Ready,
            desired,
            session,
            sessions_since_start: 4,
            recent: vec![
                SessionEnd::Succeeded,
                SessionEnd::Failed {
                    reason: "model error".to_owned(),
                },
            ],
            instruction_chars: 4,
            instruction_first_line: Some("work".to_owned()),
            activity: Some("commented #12".to_owned()),
        }
    }

    #[test]
    fn every_forgejo_state_has_a_symbol_a_word_and_no_engine_word() {
        for state in [
            ForgejoState::Missing,
            ForgejoState::Fetching { percent: Some(42) },
            ForgejoState::Preparing,
            ForgejoState::Starting,
            ForgejoState::Running,
            ForgejoState::Stopped,
            ForgejoState::Failed {
                reason: "endpoint refused the token".to_owned(),
            },
        ] {
            let face = forgejo_face(&state);
            assert!(matches!(face.symbol, '●' | '◐' | '○'));
            assert!(!face.word.is_empty());
            let written = format!("{} {}", face.word, face.status).to_ascii_lowercase();
            for word in BANNED {
                assert!(!written.contains(word), "{written} contains {word}");
            }
        }
    }

    #[test]
    fn every_agent_state_has_a_symbol_a_word_and_no_engine_word() {
        let states = [
            (
                SessionState::Running {
                    number: 1,
                    since: SystemTime::now(),
                },
                Desired::Started,
            ),
            (
                SessionState::Restarting {
                    next_at: SystemTime::now() + Duration::from_secs(8),
                    last: SessionEnd::Succeeded,
                },
                Desired::Started,
            ),
            (
                SessionState::Blocked {
                    reason: "needs an instruction".to_owned(),
                },
                Desired::Started,
            ),
            (
                SessionState::Blocked {
                    reason: "needs a model".to_owned(),
                },
                Desired::Started,
            ),
            (
                SessionState::Blocked {
                    reason: "waiting for Forgejo".to_owned(),
                },
                Desired::Started,
            ),
            (SessionState::Idle, Desired::Stopped),
            (SessionState::Idle, Desired::Started),
        ];
        for (session, desired) in states {
            let face = agent_face(&agent(session, desired));
            assert!(matches!(face.symbol, '●' | '◐' | '○'));
            let written = format!("{} {}", face.word, face.status).to_ascii_lowercase();
            for word in BANNED {
                assert!(!written.contains(word), "{written} contains {word}");
            }
        }
    }

    #[test]
    fn the_blocked_reasons_become_the_cards_words() {
        assert_eq!(
            blocked_words("needs an instruction"),
            "needs an instruction"
        );
        assert_eq!(blocked_words("needs a model"), "needs a model");
        assert_eq!(blocked_words("waiting for Forgejo"), "waiting for Forgejo");
        assert_eq!(blocked_words("account not ready"), "account not ready");
        assert_eq!(blocked_words("something else"), "needs help");
    }

    #[test]
    fn activities_are_written_out_and_free_text_is_cleaned() {
        assert_eq!(activity_words("commented #12"), "commented on #12");
        assert_eq!(activity_words("opened PR #4"), "opened pull request #4");
        assert_eq!(activity_words("commented on #12"), "commented on #12");
        assert_eq!(activity_words("opened #4"), "opened pull request #4");
        assert_eq!(activity_words("updated #4"), "updated pull request #4");
        assert!(!plain("this session token endpoint is missing and blocked").contains("session"));
        assert_eq!(
            plain("`agent-4` is connected as `gewu-agent-4`"),
            "agent-4 is connected as gewu-agent-4"
        );
    }
}
