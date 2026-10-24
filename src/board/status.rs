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

/// Remove the engine's technical words from text the product itself wrote.
///
/// The engine contract is allowed to say `endpoint`, `token`, `session` and the rest;
/// the person is not. This is the translation point for an engine-authored notice or
/// state reason. It is deliberately **not** applied to a run's own final summary: the
/// model's words are kept unchanged apart from the existing credential masking, so a
/// sentence containing an ordinary word like "missing" is never rewritten. The
/// visible row may still be shortened with an explicit ellipsis; the full sentence is
/// in the transcript.
pub fn plain(text: &str) -> String {
    text.replace("endpoint", "model")
        .replace("provisioning", "setting up")
        .replace("token", "login")
        .replace("session", "run")
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

/// A tool failure fitted to the one cause row the card can show.
///
/// The record keeps the tool's full line. At a narrow card, a long POSIX diagnostic
/// can otherwise be cut at the first space after the program (`cat:…`) and lose the
/// actual error. When that would happen, keep a truthful compact form
/// `<program>: …: <error>`; the path is still in the full record and transcript.
pub fn fit_tool_failure(text: &str, width: usize) -> String {
    if super::text::width(text) <= width {
        return text.to_owned();
    }
    let (lead, body) = match text.strip_prefix("write refused: ") {
        Some(body) => ("write refused: ", body),
        None => ("", text),
    };
    let parts: Vec<&str> = body.split(": ").collect();
    let start = if parts.len() >= 3 && parts[0] == "shell" && parts[1].starts_with("exit ") {
        2
    } else {
        0
    };
    if start >= parts.len() {
        return text.to_owned();
    }
    let diagnostic = parts[start..].join(": ");
    let Some(compact) = crate::agent_run::compact_diagnostic(&diagnostic) else {
        return text.to_owned();
    };
    let prefix = parts[..start].join(": ");
    if prefix.is_empty() {
        format!("{lead}{compact}")
    } else {
        format!("{lead}{prefix}: {compact}")
    }
}

/// The blocked reason in the words the card uses.
///
/// Unknown reasons are shortened rather than shown raw: a reason is one short line,
/// and it must not carry an engine word onto the screen.
pub fn blocked_words(reason: &str) -> &'static str {
    let reason = reason.to_ascii_lowercase();
    // The three cleanup states keep their own words, so an unfinished stop never
    // reads as a generic `needs help` or as a live process when it is only unproven.
    if reason.trim_start().starts_with("cleanup unverified:") {
        "cleanup unverified"
    } else if reason.trim_start().starts_with("unverified leftover:") {
        "unverified leftover"
    } else if reason.trim_start().starts_with("live leftover:") {
        "live leftover"
    } else if reason.contains("instruction") {
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
///
/// Only the product's own activity prefix is rewritten (`opened PR #4` becomes the
/// same sentence at every size); the repository or branch text that follows it is
/// kept as the record wrote it. This is not the engine-word map: an activity is a
/// fact about the run, not engine prose.
pub fn activity_words(activity: &str) -> String {
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
    activity.to_owned()
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

/// The shortest activity words for a card row while keeping the action and number.
fn compact_activity_words(activity: &str) -> String {
    if let Some(rest) = activity.strip_prefix("opened pull request ") {
        return format!("opened {rest}");
    }
    if let Some(rest) = activity.strip_prefix("updated pull request ") {
        return format!("updated {rest}");
    }
    if let Some(rest) = activity.strip_prefix("commented on ") {
        return format!("comment {rest}");
    }
    activity.to_owned()
}

/// The newest activity and its age, fitted to a roomy card's body row.
///
/// The full documented form (`last: … · 2m ago`) is used when the card has room. A
/// narrower card (the 120×40 and 200×60 twelve-agent grids) takes the compact action
/// words, and only the action is cut when even that is too wide: the age suffix is
/// reserved first, so a truthful age is never ellipsized away from its action.
pub fn activity_in(agent: &AgentView, width: usize) -> Option<String> {
    let full = activity(agent)?;
    if super::text::width(&full) <= width {
        return Some(full);
    }
    if matches!(agent.session, SessionState::Running { .. }) {
        // A running tool row is `shell · 3s`; there is no separate age suffix.
        return Some(super::text::ellipsize_words(&full, width));
    }
    let words = agent
        .activity
        .as_deref()
        .map(activity_words)
        .map(|activity| compact_activity_words(&activity))?;
    let suffix = match agent.activity_at {
        Some(at) => {
            let age = SystemTime::now()
                .duration_since(at)
                .map(age_words_short)
                .unwrap_or_else(|_| "now".to_owned());
            format!(" · {age}")
        }
        None => String::new(),
    };
    Some(fit_activity(&words, &suffix, width))
}

/// Fit an activity action while reserving the age suffix at the end.
///
/// The candidates give up only words, never the action's number or the age: first
/// the full `last: …` form, then the shorter `last …` label, then the unlabelled
/// action, then just its verb and number. A final ellipsis may shorten the action,
/// but the suffix width is always subtracted first. The resulting line is always
/// within `width` cells and, whenever the suffix itself fits, still carries the age.
fn fit_activity(words: &str, suffix: &str, width: usize) -> String {
    let tight = tight_activity_words(words);
    let number = words
        .split_whitespace()
        .find(|word| word.starts_with('#'))
        .unwrap_or("");
    let candidates = [
        format!("last: {words}{suffix}"),
        format!("last {words}{suffix}"),
        format!("{words}{suffix}"),
        format!("{tight}{suffix}"),
        if number.is_empty() {
            format!("{words}{suffix}")
        } else {
            format!("{number}{suffix}")
        },
    ];
    for candidate in candidates {
        if super::text::width(&candidate) <= width {
            return candidate;
        }
    }
    let room = width.saturating_sub(super::text::width(suffix));
    if room == 0 {
        return super::text::ellipsize(suffix.trim_start(), width);
    }
    let action = super::text::ellipsize_words(&format!("last: {words}"), room);
    format!("{action}{suffix}")
}

/// The verb and numeric identity of an activity, without the longer object words.
fn tight_activity_words(words: &str) -> String {
    let mut parts = words.split_whitespace();
    let Some(verb) = parts.next() else {
        return String::new();
    };
    match parts.find(|word| word.starts_with('#')) {
        Some(number) => format!("{verb} {number}"),
        None => verb.to_owned(),
    }
}

/// An age in the card's few words.
fn age_words_short(age: std::time::Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 10 {
        "now".to_owned()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

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

/// The first clause of a run's own sentence, as a card line.
///
/// The words are kept unchanged (apart from the credential masking the run already
/// did) and only the tail after the first sentence is dropped. The engine-word map is
/// not applied here: a model that wrote "the missing file" keeps those exact words
/// (F3). The display row may shorten the result with an explicit ellipsis; the
/// transcript keeps the whole sentence.
pub fn first_clause(text: &str) -> String {
    let mut cut = text.len();
    for needle in [" — ", ". ", "; ", "\n"] {
        if let Some(at) = text.find(needle) {
            cut = cut.min(at);
        }
    }
    let clause = text[..cut].trim().trim_end_matches('.');
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
    if agent.leftover.is_some() {
        // A cleanup that could not be proven is not the run's own `✗`; the card
        // names it separately so an old failed mark is never borrowed for it.
        '⚠'
    } else if matches!(agent.recent.last(), Some(SessionEnd::Refused { .. }))
        && agent
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

/// The category the recovery boundary wrote in the reason.
///
/// Only the exact category prefix decides the state. A phrase such as "cannot be
/// verified" can occur in all three reasons; ordinary substrings must never relabel
/// record uncertainty as a proven live leftover.
fn leftover_category(reason: &str) -> u8 {
    let reason = reason.trim_start();
    if reason.starts_with("live leftover:") {
        0
    } else if reason.starts_with("unverified leftover:") {
        1
    } else {
        2
    }
}

/// The card's word for a cleanup the engine could not finish.
///
/// Three states are kept apart: a known-live process that resisted stopping is
/// `live leftover`; a live process whose ownership could not be proven is
/// `unverified leftover`; a run that left no ownership record is `cleanup
/// unverified`. None of them is a plain `stopped` or the run's own failure mark.
pub fn leftover_words(leftover: &str) -> &'static str {
    match leftover_category(leftover) {
        0 => "⚠ live leftover",
        1 => "⚠ unverified leftover",
        _ => "⚠ cleanup unverified",
    }
}

/// The one-letter cleanup category used on a mixed compact line; the list legend
/// expands `u`/`o`/`l`.
pub fn leftover_letter(leftover: &str) -> &'static str {
    match leftover_category(leftover) {
        0 => "l",
        1 => "o",
        _ => "u",
    }
}

/// A two-cell cleanup category for the floor list when a persisted login takes the
/// remaining room. The list legend expands it to the full short word.
pub fn leftover_code(leftover: &str) -> &'static str {
    match leftover_category(leftover) {
        0 => "⚠l",
        1 => "⚠o",
        _ => "⚠u",
    }
}

/// The shortest form of [`leftover_words`] that keeps the three states apart on a
/// tight card border.
pub fn leftover_short(leftover: &str) -> &'static str {
    match leftover_category(leftover) {
        0 => "⚠ live",
        1 => "⚠ unowned",
        _ => "⚠ unverified",
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

/// The colour of a run's own result or activity row.
pub fn result_colour(agent: &AgentView) -> Color {
    if run_result_is_refusal(agent) {
        theme::RED
    } else if run_result_is_warning(agent) {
        theme::AMBER
    } else {
        theme::MUTED
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

/// The last-work identity (`#4`, `#3` or a short branch) without letting a later
/// failure replace it. The compact floor uses this beside a cleanup code.
pub fn last_work_identity(agent: &AgentView) -> Option<String> {
    let mut best: Option<String> = None;
    for line in agent.last_work.iter().rev() {
        if let Some(rest) = line.strip_prefix("pull request #") {
            return Some(format!("#{}", leading_number(rest)));
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

/// `✗ HTTP 500` / `✗ model unreachable` for the newest failed run. `None` when the
/// newest run did not fail.
pub fn newest_failure_word(agent: &AgentView) -> Option<String> {
    if let Some(leftover) = agent.leftover.as_deref() {
        return Some(leftover_words(leftover).to_owned());
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
    if let Some(leftover) = agent.leftover.as_deref() {
        return Some(leftover_short(leftover).to_owned());
    }
    if let Some(SessionEnd::Failed { reason }) = agent.recent.last()
        && let Some(code) = http_status_in(reason)
    {
        return Some(format!("✗ HTTP {code}"));
    }
    newest_failure_word(agent)
}

/// The one word a two-column list line can hold.
///
/// It is the compact cleanup state, the short failure word, or the last-work word.
/// The name, count and this word stay on a line whose login may have to give way;
/// the Agent screen and footer keep the longer form.
pub fn list_word(agent: &AgentView) -> Option<String> {
    if let Some(leftover) = agent.leftover.as_deref() {
        // The compact line keeps the distinction (`live` versus `unverified`); the
        // Agent screen and footer carry the full state sentence.
        return Some(leftover_short(leftover).to_owned());
    }
    if let Some(SessionEnd::Failed { reason }) = agent.recent.last() {
        return Some(format!("✗ {}", short_failure(reason)));
    }
    last_work_word(agent)
}

/// The full failure word for a board card: `✗ HTTP 500 from the model` or
/// `✗ model unreachable`. The list form uses [`newest_failure_word`]'s shorter
/// spelling because a line has room for one work word, not a sentence (W67-3).
pub fn card_word(agent: &AgentView) -> Option<String> {
    if let Some(leftover) = agent.leftover.as_deref() {
        return Some(leftover_words(leftover).to_owned());
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
    // This branch is engine prose (a provider/transport sentence), not the agent's
    // own final summary, so the engine-word map still applies.
    plain(&first_clause(reason))
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
            program: "builtin".to_owned(),
            program_label: "built-in engine".to_owned(),
            program_installed: true,
            program_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            protocol: "chat-completions".to_owned(),
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            model: "lab-chat-1".to_owned(),
            key_present: true,
            program_error: None,
            active_program: None,
            next_program: None,
            codex_sandbox: None,
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
    fn activities_are_written_out_and_engine_prose_is_cleaned() {
        assert_eq!(activity_words("commented #12"), "commented on #12");
        assert_eq!(activity_words("opened PR #4"), "opened pull request #4");
        assert_eq!(activity_words("commented on #12"), "commented on #12");
        assert_eq!(activity_words("opened #4"), "opened pull request #4");
        assert_eq!(activity_words("updated #4"), "updated pull request #4");
        assert!(!plain("this session token endpoint is blocked").contains("session"));
        assert_eq!(
            plain("`agent-4` is connected as `gewu-agent-4`"),
            "agent-4 is connected as gewu-agent-4"
        );
    }

    #[test]
    fn activity_in_keeps_the_age_after_cutting_the_action() {
        let mut agent = agent(SessionState::Idle, Desired::Stopped);
        agent.activity = Some("opened pull request #20".to_owned());
        agent.activity_at = Some(SystemTime::now() - Duration::from_secs(7200));
        agent.last_result = None;
        agent.last_work_result = None;
        for width in [18, 24, 28, 36, 60] {
            let line = activity_in(&agent, width).expect("an activity line");
            assert!(
                super::super::text::width(&line) <= width,
                "width {width}: the line overflows: {line:?}"
            );
            assert!(
                line.contains("2h"),
                "width {width}: the age was cut from the activity: {line:?}"
            );
            if width >= 24 {
                assert!(
                    line.contains("#20"),
                    "width {width}: the action identity was lost: {line:?}"
                );
            }
        }
    }

    #[test]
    fn cleanup_categories_use_their_own_prefix_not_a_shared_phrase() {
        // Every one of these contains "cannot be verified"; only the exact prefix
        // may decide the state.
        let cleanup = "cleanup unverified: the run's ending cannot be verified: permission denied";
        let unowned = "unverified leftover: a live process group cannot be verified as this run's";
        let live = "live leftover: a run process (pid 4) could not be stopped";
        assert_eq!(leftover_words(cleanup), "⚠ cleanup unverified");
        assert_eq!(leftover_short(cleanup), "⚠ unverified");
        assert_eq!(blocked_words(cleanup), "cleanup unverified");
        assert_eq!(leftover_words(unowned), "⚠ unverified leftover");
        assert_eq!(leftover_short(unowned), "⚠ unowned");
        assert_eq!(blocked_words(unowned), "unverified leftover");
        assert_eq!(leftover_words(live), "⚠ live leftover");
        assert_eq!(leftover_short(live), "⚠ live");
        assert_eq!(blocked_words(live), "live leftover");
    }

    #[test]
    fn a_run_summary_keeps_its_words_and_status_labels_are_exact() {
        // F3: the engine-word/status map is not applied to the model's own sentence.
        assert_eq!(
            first_clause("I could not read the missing file."),
            "I could not read the missing file"
        );
        assert_eq!(
            first_clause("The endpoint session token is missing."),
            "The endpoint session token is missing"
        );
        assert_eq!(blocked_words("cleanup unverified: x"), "cleanup unverified");
        assert_eq!(blocked_words("live leftover: x"), "live leftover");
        assert_eq!(
            blocked_words("unverified leftover: x"),
            "unverified leftover"
        );
        assert_eq!(blocked_words("blocked"), "needs help");
        assert_eq!(
            blocked_words("needs an instruction"),
            "needs an instruction"
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
