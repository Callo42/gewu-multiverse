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
/// the person is not. This is the translation point for an engine free-text reason or
/// an activity string, so no such word can leak through a notice or a reason.
pub fn plain(text: &str) -> String {
    text.replace("endpoint", "model")
        .replace("provisioning", "setting up")
        .replace("token", "login")
        .replace("session", "run")
        .replace("missing", "not set up")
        .replace("blocked", "stopped")
        .replace('`', "")
}

/// A tool's own failure line, kept as the tool printed it.
///
/// The run record already redacts the run's key. The engine's word list is not
/// applied here: a tool that says `cat: missing.txt: …` names the file it tried to
/// read, and `cat: not set up.txt: …` would name a different file (correction 8,
/// NB1). The row is a tool's evidence, not engine prose.
pub fn tool_failure(text: &str) -> String {
    text.to_owned()
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
        SessionState::Restarting { next_at, last } => {
            let seconds = next_at
                .duration_since(SystemTime::now())
                .map(|left| left.as_secs().max(1))
                .unwrap_or(1);
            // A no-work run backs off further than an ordinary idle. The card says
            // why in the same line, so the longer wait is never a mystery (W67-12).
            let status = if matches!(last, SessionEnd::NoWork { .. }) {
                format!("next run in {seconds}s · no work")
            } else {
                format!("next run in {seconds}s")
            };
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
                // An unfilled ring: a blocked agent is not working, and must not wear
                // the filled circle a working run wears (F6).
                symbol: '○',
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
///
/// While a tool call is in flight that is what the card says, with the seconds it has
/// been waiting (P2). After the run ends the line is marked `last:` with its age, so
/// it cannot read as the current thing (P10).
pub fn activity(agent: &AgentView) -> Option<String> {
    if matches!(agent.session, SessionState::Running { .. }) {
        if let Some(tool) = agent
            .current_tool
            .as_deref()
            .filter(|tool| !tool.trim().is_empty())
        {
            return Some(format!(
                "{} · {}s",
                tool.trim(),
                agent.current_tool_seconds.max(1)
            ));
        }
        return agent.activity.as_deref().map(activity_words);
    }
    let activity = agent.activity.as_deref().map(activity_words)?;
    match agent.activity_at {
        Some(at) => {
            let age = SystemTime::now()
                .duration_since(at)
                .map(age_words)
                .unwrap_or_else(|_| "just now".to_owned());
            Some(format!("last: {activity} · {age}"))
        }
        None => Some(format!("last: {activity}")),
    }
}

/// An age in the card's few words.
fn age_words(age: std::time::Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 10 {
        "just now".to_owned()
    } else if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}

/// The first clause of an error, as a card line: no raw body, no second sentence.
pub fn first_clause(text: &str) -> String {
    let plain = plain(text);
    let mut cut = plain.len();
    for needle in [" — ", ". ", "; ", "\n"] {
        if let Some(at) = plain.find(needle) {
            cut = cut.min(at);
        }
    }
    let clause = plain[..cut].trim().trim_end_matches('.');
    if clause.chars().count() <= 90 {
        return clause.to_owned();
    }
    let whole: String = clause.chars().take(89).collect();
    let cut = whole.rfind(' ').unwrap_or(whole.len());
    format!("{}…", whole[..cut].trim_end())
}

/// The run count shown on the agent panel: `run 4`.
///
/// This is the total from the run records, not the session counter: it does not reset
/// when the agent starts again or the program reopens (W31-5, F5).
pub fn run_count(agent: &AgentView) -> String {
    format!("run {}", agent.runs_total)
}

/// The symbol and colour of one finished run.
pub fn session_symbol(end: &SessionEnd) -> (char, Color) {
    match end {
        SessionEnd::Succeeded => ('✓', theme::RUNNING),
        SessionEnd::Refused { .. } => ('⊘', theme::RED),
        SessionEnd::NoWork { .. } => ('∅', theme::AMBER),
        // A clean run whose tool results include a failed command: never a green
        // tick, and not a refusal either (correction 4).
        SessionEnd::Warned { .. } => ('⚠', theme::AMBER),
        SessionEnd::Failed { .. } => ('✗', theme::RED),
        SessionEnd::TimedOut => ('◷', theme::AMBER),
        SessionEnd::Stopped => ('■', theme::GREY),
    }
}

/// The mark beside the newest failed tool call.
///
/// `✗` belongs to a run that itself failed. A refused **write** is the strip's `⊘`,
/// so its cause row wears the same mark: `⊘ write refused: …`. Every other failed
/// command inside a clean (or refused) run is a `⚠`; a refused read is never the
/// write mark (F-02, U6). The strip still shows the history, so a previous run's `✓`
/// may stand beside the newest run's `✗ shell: …`; the newest mark and its row are
/// what the failed call gets (correction 5, N2).
pub fn failure_mark(agent: &AgentView) -> char {
    if agent
        .last_failure
        .as_deref()
        .is_some_and(|failure| failure.trim_start().starts_with("write refused:"))
    {
        '⊘'
    } else if matches!(agent.recent.last(), Some(SessionEnd::Failed { .. })) {
        '✗'
    } else {
        '⚠'
    }
}

/// The one-word label for a run mark, used beside the newest mark so the two circle
/// glyphs never have to be told apart from the legend alone (F-04).
pub fn session_label(end: &SessionEnd) -> &'static str {
    match end {
        SessionEnd::Succeeded => "ok",
        SessionEnd::Refused { .. } => "refused",
        SessionEnd::NoWork { .. } => "no work",
        SessionEnd::Warned { .. } => "cmd failed",
        SessionEnd::Failed { .. } => "failed",
        SessionEnd::TimedOut => "timed out",
        SessionEnd::Stopped => "stopped",
    }
}

/// The newest finished run's line, labelled by what it is.
///
/// The line prefers the newest run that did work: a later check run's no-op reason
/// must not stand for the agent's whole work (F-10). A no-op's own reason is labelled
/// `no work:` instead of being presented as the result. The line is drawn beside
/// every finished mark — `✓` included — so the symbol is never the whole claim
/// (review B2).
pub fn run_result_line(agent: &AgentView) -> Option<String> {
    if matches!(agent.session, SessionState::Running { .. }) {
        return None;
    }
    let newest = agent.recent.last()?;
    if !agent.last_result_is_work
        && let Some(work) = agent.last_work_result.as_deref()
    {
        return Some(format!("last work: {}", first_clause(work)));
    }
    let result = agent.last_result.as_deref().map(first_clause);
    match newest {
        SessionEnd::Succeeded | SessionEnd::Warned { .. } => {
            result.map(|result| format!("result: {result}"))
        }
        SessionEnd::Refused { reason } => Some(format!(
            "result: {}",
            result.unwrap_or_else(|| first_clause(reason))
        )),
        SessionEnd::NoWork { reason } => Some(format!(
            "no work: {}",
            result.unwrap_or_else(|| first_clause(reason))
        )),
        _ => None,
    }
}

/// Whether the result row belongs to the warned mark, so it is drawn as a caution
/// (amber) rather than ordinary detail beside a `✓`.
pub fn run_result_is_warning(agent: &AgentView) -> bool {
    matches!(agent.recent.last(), Some(SessionEnd::Warned { .. }))
}

/// Whether the result row is a refusal/no-work, so it is drawn as a caution and not
/// as ordinary detail beside a `✓`.
pub fn run_result_is_refusal(agent: &AgentView) -> bool {
    matches!(
        agent.recent.last(),
        Some(SessionEnd::Refused { .. } | SessionEnd::NoWork { .. })
    )
}

/// The one word a compact board card shows for the newest run's failure.
///
/// An outage must be visible without opening the agent: the newest failed run
/// replaces the last-work word with `✗ HTTP 500` (or `✗ model unreachable`).
/// Refused and warned runs keep their last-work word; the strip and the panel
/// still carry their mark (W67-3).
pub fn last_work_word(agent: &AgentView) -> Option<String> {
    if let Some(word) = newest_failure_word(agent) {
        return Some(word);
    }
    let mut best: Option<String> = None;
    for line in agent.last_work.iter().rev() {
        if let Some(rest) = line.strip_prefix("pull request #") {
            // The number is the fact a person needs; the label is what gives way
            // when a compact line is tight. `PR #100` stays `#100`, never `PR#10`.
            best = Some(format!("#{}", leading_number(rest)));
            break;
        }
        if let Some(rest) = line.strip_prefix("issue #") {
            best = Some(format!("#{}", leading_number(rest)));
        } else if let Some(rest) = line.strip_prefix("comment on #") {
            best.get_or_insert_with(|| format!("#{}", leading_number(rest)));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let branch = rest.split_whitespace().next().unwrap_or(rest);
            best.get_or_insert_with(|| branch.chars().take(7).collect());
        }
    }
    best
}

fn leading_number(text: &str) -> String {
    text.chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
}

/// Whether the compact card's word is a newest-run failure, so it is drawn red.
pub fn last_work_is_failure(agent: &AgentView) -> bool {
    newest_failure_word(agent).is_some()
}

/// `✗ HTTP 500` / `✗ model unreachable` for the newest failed run. `None` when the
/// newest run did not fail.
pub fn newest_failure_word(agent: &AgentView) -> Option<String> {
    if agent.leftover.is_some() {
        return Some("⚠ leftover".to_owned());
    }
    match agent.recent.last() {
        Some(SessionEnd::Failed { reason }) => Some(format!("✗ {}", short_failure(reason))),
        Some(SessionEnd::TimedOut) => Some("◷ timeout".to_owned()),
        _ => None,
    }
}

/// The shortest failure word that still carries an HTTP status, for the one
/// three-row card form whose bottom border must also hold the state word:
/// `✗ HTTP 500`. Other failures reuse [`newest_failure_word`] (`✗ net`,
/// `◷ timeout`, `⚠ leftover`).
pub fn tight_failure_word(agent: &AgentView) -> Option<String> {
    if let Some(SessionEnd::Failed { reason }) = agent.recent.last()
        && let Some(code) = http_status_in(reason)
    {
        return Some(format!("✗ HTTP {code}"));
    }
    newest_failure_word(agent)
}

/// The full failure word for a board card: `✗ HTTP 500 from the model` or
/// `✗ model unreachable`. The list form uses [`newest_failure_word`]'s shorter
/// spelling because a line has room for one work word, not a sentence (W67-3).
pub fn card_word(agent: &AgentView) -> Option<String> {
    if agent.leftover.is_some() {
        return Some("⚠ leftover process".to_owned());
    }
    if let Some(SessionEnd::Failed { reason }) = agent.recent.last() {
        return Some(format!("✗ {}", failure_words(reason)));
    }
    last_work_word(agent)
}

fn short_failure(reason: &str) -> String {
    if let Some(code) = http_status_in(reason) {
        return code.to_string();
    }
    if reason
        .to_ascii_lowercase()
        .contains("cannot reach the model")
    {
        return "net".to_owned();
    }
    "failed".to_owned()
}

/// One whole-word failure line for a card: keep the HTTP status first (the fact a
/// person needs to tell a provider problem from a bad model name), then the
/// shortest honest phrase for the transport cases.
pub fn failure_words(reason: &str) -> String {
    if let Some(code) = http_status_in(reason) {
        return format!("HTTP {code} from the model");
    }
    let lower = reason.to_ascii_lowercase();
    if lower.contains("cannot reach the model") {
        if let Some(errno) = errno_in(reason) {
            return format!("model unreachable (errno {errno})");
        }
        return "model unreachable".to_owned();
    }
    if lower.contains("no answer within") {
        return "model timed out".to_owned();
    }
    if lower.contains("bad_key") || lower.contains("refused the api key") {
        return "the API key was refused".to_owned();
    }
    if lower.contains("model_not_found") {
        return "the model name was not found".to_owned();
    }
    first_clause(reason)
}

fn http_status_in(text: &str) -> Option<u16> {
    for marker in ["HTTP ", "HTTP/1.1 ", "HTTP/1.0 ", "status "] {
        if let Some(rest) = text.split(marker).nth(1) {
            let digits: String = rest
                .chars()
                .skip_while(|ch| !ch.is_ascii_digit())
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            if let Ok(code) = digits.parse::<u16>()
                && (400..=599).contains(&code)
            {
                return Some(code);
            }
        }
    }
    None
}

fn errno_in(text: &str) -> Option<String> {
    let at = text.find("errno ")? + "errno ".len();
    let digits: String = text[at..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
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
            runs_total: 4,
            recent: vec![
                SessionEnd::Succeeded,
                SessionEnd::Failed {
                    reason: "model error".to_owned(),
                },
            ],
            instruction_chars: 4,
            instruction_first_line: Some("work".to_owned()),
            instruction: Some("work".to_owned()),
            instruction_lines: 1,
            activity: Some("commented #12".to_owned()),
            activity_at: Some(SystemTime::now() - Duration::from_secs(120)),
            last_error: None,
            last_result: None,
            last_failure: None,
            leftover: None,
            current_tool: None,
            current_tool_seconds: 0,
            last_work: Vec::new(),
            no_assigned_work: false,
            last_work_result: None,
            last_result_is_work: false,
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
    fn a_blocked_agent_wears_an_unfilled_ring_not_the_working_circle() {
        let face = agent_face(&agent(
            SessionState::Blocked {
                reason: "needs an instruction".to_owned(),
            },
            Desired::Started,
        ));
        assert_eq!(face.symbol, '○');
        assert_eq!(face.word, "needs an instruction");
        let working = agent_face(&agent(
            SessionState::Running {
                number: 1,
                since: SystemTime::now(),
            },
            Desired::Started,
        ));
        assert_eq!(working.symbol, '●');
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

    #[test]
    fn a_tool_failure_keeps_the_tool_words() {
        // Correction 8, NB1: the failure row is the tool's own diagnostic. The
        // engine-word translation turned `cat: missing.txt` into a different file
        // name on the card; a tool's file name is not engine prose.
        assert_eq!(
            tool_failure("shell: cat: missing.txt: No such file or directory"),
            "shell: cat: missing.txt: No such file or directory"
        );
        assert_eq!(
            tool_failure("shell: token.txt: Permission denied"),
            "shell: token.txt: Permission denied"
        );
    }

    #[test]
    fn the_card_reads_the_total_run_count_not_the_session_counter() {
        let mut agent = agent(SessionState::Idle, Desired::Stopped);
        agent.sessions_since_start = 1;
        agent.runs_total = 7;
        assert_eq!(run_count(&agent), "run 7");
        let refused = SessionEnd::Refused {
            reason: "I could not do it".to_owned(),
        };
        assert_eq!(session_symbol(&refused).0, '⊘');
        assert_eq!(
            session_symbol(&SessionEnd::NoWork {
                reason: "nothing assigned".to_owned(),
            })
            .0,
            '∅'
        );
    }
}
