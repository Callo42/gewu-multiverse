//! Per-harness terminal-outcome model for a native run.
//!
//! A native harness is not one generic channel. Each program has its own terminal
//! contract:
//!
//! - Pi exits 0 on provider failures; `auto_retry_end` is the terminal pair and
//!   `message_end.errorMessage` is a per-attempt signal.
//! - Codex `turn.completed` / `turn.failed` is the terminal pair; a top-level
//!   `error` is retry chatter.
//! - DSH `status` with `phase:"turn_end"` and `reason` is authoritative; `final`
//!   is only a projection.
//! - OpenCode has no terminal event: an `error` is a failed attempt and the
//!   harness's own exit status decides whether a later attempt recovered.
//!
//! This module keeps one ordered failure timeline per run and derives the outcome
//! from the harness's contract. A transient failure is recorded but cannot end a
//! recovered run; the first terminal failure supplies the run's reason. Tool
//! results are separate from the command's success: a failed tool warns the run
//! but does not turn a successful turn into a failed session.
//!
//! The parser is pure: `observe_line` consumes one JSON event and `finish` takes
//! the process ending plus the supervisor's ending (natural, stop, deadline). The
//! same functions are used by the live supervisor and by the replay corpus test.

use serde_json::Value;

use crate::harness::Program;

/// A provider failure with every detail the harness emitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProviderDetail {
    pub status: Option<u16>,
    pub kind: Option<String>,
    pub code: Option<String>,
    pub message: String,
    pub url: Option<String>,
}

impl ProviderDetail {
    pub(super) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// Extract the provider fields a harness embedded in a prose message or a raw
    /// JSON error body. Nothing is invented: a field the harness did not emit stays
    /// absent.
    pub(super) fn from_text(text: &str) -> Self {
        let text = text.trim();
        let mut detail = Self::message(text);
        if let Some(status) = status_in(text) {
            detail.status = Some(status);
        }
        if let Some((kind, code, message)) = json_error_fields(text) {
            detail.kind = kind;
            detail.code = code;
            if !message.trim().is_empty() {
                detail.message = message;
            }
        }
        if let Some(url) = url_in(text) {
            detail.url = Some(url);
        }
        // Pi's `401: {...}` carries the status before the body.
        if detail.status.is_none()
            && let Some((status, rest)) = leading_status(text)
        {
            detail.status = Some(status);
            if let Some((kind, code, message)) = json_error_fields(rest) {
                detail.kind = kind;
                detail.code = code;
                if !message.trim().is_empty() {
                    detail.message = message;
                }
            } else if detail.message == text {
                detail.message = rest.trim().to_owned();
            }
        }
        detail
    }

    pub(super) fn reason(&self) -> String {
        let message = self.message.trim();
        let mut head = String::new();
        if let Some(code) = self.code.as_deref().filter(|code| !code.is_empty()) {
            head.push_str(code);
            head.push_str(": ");
        }
        if let Some(status) = self.status
            && !message.contains(&status.to_string())
        {
            head.push_str("HTTP ");
            head.push_str(&status.to_string());
            head.push_str(": ");
        }
        let mut out = format!("{head}{message}");
        if let Some(kind) = self.kind.as_deref().filter(|kind| !kind.is_empty())
            && !out.contains(kind)
        {
            out.push_str(&format!(" ({kind})"));
        }
        if out.trim().is_empty() {
            "the native provider reported a failure without a message".to_owned()
        } else {
            out
        }
    }
}

fn status_in(text: &str) -> Option<u16> {
    for needle in [
        "unexpected status ",
        "last status: ",
        "status: ",
        "status_code=",
        "status=",
        "HTTP ",
    ] {
        if let Some(at) = text.find(needle) {
            let rest = &text[at + needle.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if (3..=3).contains(&digits.len())
                && let Ok(status) = digits.parse::<u16>()
            {
                return Some(status);
            }
        }
    }
    None
}

fn leading_status(text: &str) -> Option<(u16, &str)> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() != 3 {
        return None;
    }
    let rest = text.get(digits.len()..)?;
    let rest = rest.strip_prefix(':')?;
    Some((digits.parse().ok()?, rest))
}

fn url_in(text: &str) -> Option<String> {
    let at = text.find("url: ")?;
    let url = text[at + 5..].split_whitespace().next()?;
    url.starts_with("http").then(|| url.to_owned())
}

/// Pull `error.message`, `error.type` and `error.code` out of a JSON error body.
fn json_error_fields(text: &str) -> Option<(Option<String>, Option<String>, String)> {
    let start = text.find('{')?;
    let value: Value = serde_json::from_str(&text[start..]).ok()?;
    let error = value.get("error").unwrap_or(&value);
    let message = error
        .get("message")
        .or_else(|| error.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let kind = error.get("type").and_then(Value::as_str).map(str::to_owned);
    let code = error
        .get("code")
        .and_then(|code| match code {
            Value::String(code) => Some(code.clone()),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        })
        .or_else(|| value.get("code").and_then(Value::as_str).map(str::to_owned));
    Some((kind, code, message))
}

/// One line of a harness's own JSON stream, reduced to the outcome-relevant fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NativeEvent {
    ToolStarted {
        id: String,
        tool: String,
        args: String,
    },
    ToolFinished {
        id: String,
        tool: Option<String>,
        exit: Option<i32>,
        signal: Option<String>,
        output: String,
    },
    /// Text from an attempt. `successful` is true only when the harness marked
    /// that attempt successful (Pi's assistant message with no error).
    Text { text: String, successful: bool },
    /// The harness marked this attempt successful without ending the turn
    /// (Pi's assistant message with no error). It may carry text through `Text`.
    AttemptSucceeded,
    /// The harness's own successful terminal event.
    TerminalSuccess,
    /// The harness's own terminal failure.
    TerminalFailure { detail: ProviderDetail },
    /// A failure the harness will retry. It never ends a recovered run.
    AttemptFailure {
        detail: ProviderDetail,
        waiting: bool,
    },
    /// The harness ended its turn.
    Settled { aborted: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SupervisorEnd {
    Natural,
    Stopped,
    Deadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Disposition {
    Succeeded,
    Failed,
    Stopped,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resolution {
    pub disposition: Disposition,
    pub reason: Option<String>,
    pub text: Option<String>,
    pub first_tool_failure: Option<String>,
    pub failed_call: Option<String>,
    pub refused_write: Option<String>,
    pub saw_tool: bool,
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressKind {
    Attempt,
    Terminal,
}

#[derive(Debug, Clone)]
struct Progress {
    kind: ProgressKind,
    cause: String,
}

#[derive(Debug, Default)]
pub(super) struct NativeOutcome {
    program: Option<Program>,
    terminal_success: bool,
    terminal_failure: Option<ProviderDetail>,
    attempt_failures: Vec<ProviderDetail>,
    waiting: Option<String>,
    /// Text already committed by a terminal successful attempt.
    committed_text: Vec<String>,
    /// Text of the current attempt when the harness itself marked that message
    /// successful (Pi). It is committed only by the terminal success; an
    /// `AttemptFailure` clears it.
    successful_text: Option<String>,
    /// Text of the current attempt for the harnesses that mark success at the
    /// terminal event rather than on the message (Codex, DSH, OpenCode). It is
    /// cleared by an `AttemptFailure` and committed only by a terminal success.
    pending_text: Option<String>,
    tool_failures: Vec<String>,
    /// The first failed tool's diagnostic. A tool warning is attached to a run,
    /// never used as a session-level or deadline failure cause.
    tool_warning: Option<String>,
    first_tool_failure: Option<String>,
    failed_call: Option<String>,
    refused_write: Option<String>,
    open: Vec<(String, String, String)>,
    saw_tool: bool,
    settled: bool,
    aborted: bool,
    /// Whether the current attempt was marked successful, even if it had no text.
    successful_attempt: bool,
    progress: Option<Progress>,
}

impl NativeOutcome {
    pub(super) fn new(program: Program) -> Self {
        Self {
            program: Some(program),
            ..Self::default()
        }
    }

    pub(super) fn program(&self) -> Option<Program> {
        self.program
    }

    /// Feed one raw JSON line. Multiple harness facts can live in one line (for
    /// example a tool call plus its result), so this returns every event the line
    /// carried.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn observe_line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        for event in events(self.program, &value) {
            self.observe(event);
        }
    }

    pub(super) fn observe(&mut self, event: NativeEvent) {
        match event {
            NativeEvent::ToolStarted { id, tool, args } => {
                // A tool call ends the current assistant message's candidacy: its
                // text belongs to an earlier, non-terminal turn. Only text from a
                // later successful attempt may be committed.
                self.successful_text = None;
                self.pending_text = None;
                self.successful_attempt = false;
                self.saw_tool = true;
                if !id.is_empty() && !self.open.iter().any(|(open_id, _, _)| *open_id == id) {
                    self.open.push((id, tool, args));
                }
            }
            NativeEvent::ToolFinished {
                id,
                tool,
                exit,
                signal,
                output,
            } => {
                self.saw_tool = true;
                let (tool, command) = self
                    .open
                    .iter()
                    .find(|(open_id, _, _)| *open_id == id)
                    .map(|(_, tool, args)| (tool.clone(), args.clone()))
                    .unwrap_or_else(|| (tool.unwrap_or_else(|| "tool".to_owned()), String::new()));
                self.open.retain(|(open_id, _, _)| *open_id != id);
                self.record_tool(&tool, &command, exit, signal, &output);
            }
            NativeEvent::Text { text, successful } => {
                if successful {
                    // Pi's per-attempt success: keep it for this attempt only.
                    self.successful_text = Some(text);
                    self.pending_text = None;
                } else {
                    self.pending_text = Some(text);
                }
            }
            NativeEvent::AttemptSucceeded => {
                self.successful_attempt = true;
                self.pending_text = None;
            }
            NativeEvent::TerminalSuccess => {
                self.terminal_success = true;
                self.successful_attempt = true;
                self.settled = true;
                // Only the attempt the harness marks terminally successful may
                // contribute text. A failed attempt's text was cleared by its
                // `AttemptFailure`; an empty successful attempt commits nothing.
                if let Some(text) = self
                    .successful_text
                    .take()
                    .or_else(|| self.pending_text.take())
                {
                    self.committed_text.push(text);
                }
                self.clear_waiting();
                self.progress = Some(Progress {
                    kind: ProgressKind::Terminal,
                    cause: String::new(),
                });
            }
            NativeEvent::TerminalFailure { detail } => {
                self.terminal_success = false;
                self.successful_attempt = false;
                self.settled = true;
                self.successful_text = None;
                self.pending_text = None;
                if self.terminal_failure.is_none() {
                    self.terminal_failure = Some(detail.clone());
                }
                self.clear_waiting();
                self.progress = Some(Progress {
                    kind: ProgressKind::Terminal,
                    cause: detail.reason(),
                });
            }
            NativeEvent::AttemptFailure { detail, waiting } => {
                self.successful_attempt = false;
                // This attempt failed. Its text belongs to the failed attempt and
                // must not be committed by a later terminal success.
                self.successful_text = None;
                self.pending_text = None;
                let reason = detail.reason();
                self.attempt_failures.push(detail);
                if waiting {
                    self.waiting = Some(reason.clone());
                }
                self.progress = Some(Progress {
                    kind: ProgressKind::Attempt,
                    cause: reason,
                });
            }
            NativeEvent::Settled { aborted } => {
                self.settled = true;
                self.aborted = aborted;
                if aborted {
                    if self.terminal_failure.is_none() {
                        self.clear_waiting();
                    }
                } else if self.program == Some(Program::Pi)
                    && self.terminal_failure.is_none()
                    && self.successful_attempt
                {
                    // Pi does not emit auto_retry_end on a first-try success. The
                    // settled turn after a successful assistant message is its
                    // terminal success, and only that message's text is committed.
                    self.terminal_success = true;
                    self.clear_waiting();
                    if let Some(text) = self.successful_text.take() {
                        self.committed_text.push(text);
                    }
                }
            }
        }
    }

    fn clear_waiting(&mut self) {
        self.waiting = None;
    }

    /// Read a program's own stderr warning for a temporary provider retry.
    ///
    /// Codex emits its retry warnings only on stderr when a 503 recovers; its JSON
    /// stream is silent about the attempt. The warning is the program's own fact, so
    /// the card can name the temporary condition while the retry is in flight.
    pub(super) fn observe_retry_text(&mut self, text: &str) -> bool {
        let lower = text.to_ascii_lowercase();
        let retrying = lower.contains("retrying sampling request")
            || lower.contains("temporarily unavailable")
            || lower.contains("rate limit");
        let status = status_in(text).or_else(|| {
            ["503", "502", "504", "429"]
                .into_iter()
                .find(|code| lower.contains(code))
                .and_then(|code| code.parse::<u16>().ok())
        });
        let Some(status) = status else {
            return false;
        };
        if !(retrying || matches!(status, 429 | 500 | 502 | 503 | 504)) {
            return false;
        }
        // The diagnostic can be a long tracing line. The status is the fact the
        // card needs; the program's full sentence is still in the run log.
        self.observe(NativeEvent::AttemptFailure {
            detail: ProviderDetail {
                status: Some(status),
                kind: None,
                code: None,
                message: "the model is temporarily unavailable".to_owned(),
                url: None,
            },
            waiting: true,
        });
        true
    }

    /// Record one finished tool call and return its diagnostic when it failed.
    /// Called by both the live event loop and the replay corpus, so a tool's
    /// return is interpreted once.
    pub(super) fn record_tool(
        &mut self,
        tool: &str,
        command: &str,
        exit: Option<i32>,
        signal: Option<String>,
        output: &str,
    ) -> Option<String> {
        let parsed = parse_tool_output(output, exit, signal);
        let failed = parsed.signal.is_some() || parsed.exit.is_some_and(|exit| exit != 0);
        if !failed {
            return None;
        }
        let diagnostic = tool_diagnostic(tool, command, &parsed);
        if self.first_tool_failure.is_none() {
            self.first_tool_failure = Some(diagnostic.clone());
        }
        if is_write_tool(tool) {
            if self.refused_write.is_none() {
                self.refused_write = Some(diagnostic.clone());
            }
        } else if self.failed_call.is_none() {
            self.failed_call = Some(diagnostic.clone());
        }
        self.tool_failures.push(diagnostic.clone());
        // A failed tool is a warning attached to the run. It never becomes the
        // session-level failure reason or a deadline cause: a tool's return is
        // not the command's success and not the harness's terminal contract.
        if self.tool_warning.is_none() {
            self.tool_warning = Some(diagnostic.clone());
        }
        Some(diagnostic)
    }

    /// The live transcript owns call completion; the outcome model only needs
    /// the started call's command until the result arrives.
    pub(super) fn forget_tool(&mut self, id: &str) {
        self.open.retain(|(open_id, _, _)| open_id != id);
    }

    fn terminal_reason(&self) -> Option<String> {
        let detail = self.terminal_failure.as_ref()?;
        let reason = detail.reason();
        // Pi's finalError is the last attempt's error, while the first attempt may
        // have failed for a different reason (a truncated stream followed by a
        // connection reset). The terminal cause is primary; the earlier cause is
        // kept because it is the first thing that actually failed.
        if self.program == Some(Program::Pi)
            && let Some(first) = self.attempt_failures.first().map(ProviderDetail::reason)
            && !reason.contains(&first)
            && !first.contains(&reason)
        {
            return Some(format!("{reason} (after earlier failure: {first})"));
        }
        Some(reason)
    }

    /// The reason of the first terminal failure, in event order.
    pub(super) fn terminal_failure_reason(&self) -> Option<String> {
        self.terminal_reason()
    }

    /// The first failed tool call, in event order.
    pub(super) fn first_tool_failure(&self) -> Option<&String> {
        self.first_tool_failure.as_ref()
    }

    pub(super) fn committed_texts(&self) -> &[String] {
        &self.committed_text
    }

    fn first_attempt_reason(&self) -> Option<String> {
        self.attempt_failures.first().map(ProviderDetail::reason)
    }

    /// Whether a deadline may report the harness's own failure cause rather than
    /// only "timed out". A waiting progress line is not a failure.
    pub(super) fn deadline_cause(&self) -> Option<String> {
        let progress = self.progress.as_ref()?;
        if progress.cause.trim().is_empty() {
            return None;
        }
        match progress.kind {
            ProgressKind::Attempt | ProgressKind::Terminal => Some(progress.cause.clone()),
        }
    }

    pub(super) fn final_text(&self) -> Option<String> {
        self.committed_text
            .iter()
            .rev()
            .find(|text| !text.trim().is_empty())
            .cloned()
    }

    pub(super) fn saw_tool(&self) -> bool {
        self.saw_tool
    }

    /// Resolve the run from the harness's terminal contract plus the process end
    /// and the supervisor's own ending.
    pub(super) fn finish(&mut self, exit: Option<i32>, supervisor: SupervisorEnd) -> Resolution {
        let mut resolution = Resolution {
            disposition: Disposition::Failed,
            reason: None,
            text: self.final_text(),
            first_tool_failure: self.first_tool_failure.clone(),
            failed_call: self.failed_call.clone(),
            refused_write: self.refused_write.clone(),
            saw_tool: self.saw_tool,
            terminal: self.terminal_success || self.terminal_failure.is_some(),
        };
        let terminal_reason = self.terminal_reason();
        let attempt_reason = self.first_attempt_reason();
        if supervisor == SupervisorEnd::Stopped {
            resolution.disposition = Disposition::Stopped;
            resolution.reason = Some(stop_reason(terminal_reason, self.waiting.as_deref()));
            return resolution;
        }
        if let Some(reason) = terminal_reason {
            resolution.disposition = Disposition::Failed;
            resolution.reason = Some(reason);
            return resolution;
        }
        if self.aborted {
            resolution.disposition = Disposition::Stopped;
            resolution.reason = Some("the native program stopped its own turn".to_owned());
            return resolution;
        }
        if supervisor == SupervisorEnd::Deadline && self.terminal_success {
            // The harness marked a turn successful but the process was still
            // running at the deadline; a completion event is not proof that the
            // program ended.
            resolution.disposition = Disposition::TimedOut;
            resolution.reason = Some(
                "the run reached its time limit after a completion event but before the native program exited"
                    .to_owned(),
            );
            return resolution;
        }
        if supervisor == SupervisorEnd::Natural && exit.is_some_and(|code| code != 0) {
            // The harness's own process ending is part of its contract. A turn
            // event cannot overrule a non-zero exit; the first failed attempt (if
            // any) is the truthful cause.
            resolution.disposition = Disposition::Failed;
            resolution.reason = exit_reason(exit, attempt_reason);
            return resolution;
        }
        if supervisor == SupervisorEnd::Natural && exit.is_none() && !self.terminal_success {
            resolution.disposition = Disposition::Failed;
            resolution.reason = Some("the native program was ended by a signal".to_owned());
            return resolution;
        }
        match self.program {
            Some(Program::Pi) => {
                if self.terminal_success {
                    resolution.disposition = Disposition::Succeeded;
                } else if supervisor == SupervisorEnd::Deadline {
                    return self.deadline_resolution(resolution, attempt_reason);
                } else if let Some(reason) = attempt_reason {
                    resolution.reason = Some(reason);
                } else if exit == Some(0) {
                    resolution.reason = Some(
                        "the native program exited 0 without a completion event; the run is not a success"
                            .to_owned(),
                    );
                } else {
                    resolution.reason = exit_reason(exit, None);
                }
            }
            Some(Program::Codex) => {
                if self.terminal_success {
                    resolution.disposition = Disposition::Succeeded;
                } else if supervisor == SupervisorEnd::Deadline {
                    return self.deadline_resolution(resolution, attempt_reason);
                } else {
                    resolution.reason = exit_reason(exit, attempt_reason);
                }
            }
            Some(Program::Dsh) => {
                if self.terminal_success {
                    resolution.disposition = Disposition::Succeeded;
                } else if supervisor == SupervisorEnd::Deadline {
                    return self.deadline_resolution(resolution, attempt_reason);
                } else {
                    resolution.reason = exit_reason(exit, attempt_reason);
                }
            }
            Some(Program::Opencode) => {
                // OpenCode has no terminal event. Its own exit status decides
                // whether a later attempt recovered; text alone is not success.
                if exit == Some(0) {
                    // OpenCode's own exit 0 is the terminal contract. It may have
                    // no final text; a recovered attempt without text is still a
                    // success and commits nothing.
                    resolution.disposition = Disposition::Succeeded;
                    resolution.reason = None;
                    if let Some(text) = self.pending_text.take() {
                        self.committed_text.push(text);
                    }
                    resolution.text = self.final_text();
                } else if supervisor == SupervisorEnd::Deadline {
                    return self.deadline_resolution(resolution, attempt_reason);
                } else {
                    resolution.reason = exit_reason(exit, attempt_reason);
                }
            }
            Some(Program::Builtin) | None => {
                resolution.reason = exit_reason(exit, attempt_reason);
            }
        }
        resolution
    }

    fn deadline_resolution(
        &self,
        mut resolution: Resolution,
        attempt_reason: Option<String>,
    ) -> Resolution {
        if let Some(cause) = self.deadline_cause() {
            // The harness's own failure existed before the product's deadline. Do
            // not reduce it to "timed out": report the failure and keep its cause.
            resolution.disposition = Disposition::Failed;
            resolution.reason = Some(cause);
        } else {
            resolution.disposition = Disposition::TimedOut;
            resolution.reason = Some(match self.waiting.as_deref() {
                Some(waiting) => format!(
                    "the run reached its time limit while the native program was still waiting: {waiting}"
                ),
                None => attempt_reason.unwrap_or_else(|| {
                    "the run reached its time limit before the native program emitted a terminal event"
                        .to_owned()
                }),
            });
        }
        resolution
    }

    /// The progress snapshot the supervisor persists after a state change. The
    /// engine reads it when it has to end a run from its own deadline, so the last
    /// emitted cause survives a killed child.
    pub(super) fn progress(&self) -> Option<(String, String, bool, Option<String>)> {
        let (state, cause, terminal) = match self.progress.as_ref() {
            Some(progress) => {
                let state = match progress.kind {
                    ProgressKind::Terminal => {
                        if self.terminal_failure.is_some() {
                            "failed"
                        } else {
                            "succeeded"
                        }
                    }
                    ProgressKind::Attempt => "retrying",
                };
                (
                    state.to_owned(),
                    progress.cause.clone(),
                    matches!(progress.kind, ProgressKind::Terminal),
                )
            }
            // A failed tool is a warning, not a failure: persist it even when no
            // provider attempt has emitted a cause, so the deadline can attach it
            // while the outcome stays TimedOut.
            None if self.tool_warning.is_some() => ("running".to_owned(), String::new(), false),
            None => return None,
        };
        Some((state, cause, terminal, self.tool_warning.clone()))
    }

    pub(super) fn tool_warning(&self) -> Option<&String> {
        self.tool_warning.as_ref()
    }
}

fn stop_reason(terminal_reason: Option<String>, waiting: Option<&str>) -> String {
    match (terminal_reason, waiting) {
        (Some(cause), _) => format!("the run was stopped on request after it recorded: {cause}"),
        (None, Some(waiting)) => {
            format!(
                "the run was stopped on request while the native program was waiting: {waiting}"
            )
        }
        (None, None) => "the run was stopped on request".to_owned(),
    }
}

fn exit_reason(exit: Option<i32>, attempt_reason: Option<String>) -> Option<String> {
    if let Some(reason) = attempt_reason {
        return Some(reason);
    }
    Some(match exit {
        Some(0) => "the native program exited without a completion event; the run is not a success"
            .to_owned(),
        Some(code) => format!("the native program exited with code {code}"),
        None => "the native program was ended by a signal".to_owned(),
    })
}

#[derive(Debug, Default)]
struct ParsedToolOutput {
    exit: Option<i32>,
    signal: Option<String>,
    diagnostic: String,
    sandbox: Option<String>,
}

fn parse_tool_output(output: &str, exit: Option<i32>, signal: Option<String>) -> ParsedToolOutput {
    let mut parsed = ParsedToolOutput {
        exit,
        signal,
        ..ParsedToolOutput::default()
    };
    let mut keep = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed == "[stderr]"
            || trimmed == "[stdout]"
            || trimmed.starts_with("landlock-run:")
        {
            continue;
        }
        if let Some(code) = marker_exit(trimmed) {
            if parsed.exit.is_none() {
                parsed.exit = Some(code);
            }
            continue;
        }
        if let Some(name) = marker_signal(trimmed) {
            if parsed.signal.is_none() {
                parsed.signal = Some(name);
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("[sandbox: ") {
            parsed.sandbox = Some(rest.trim_end_matches(']').trim().to_owned());
            continue;
        }
        if let Some(code) = marker_command_exited(trimmed) {
            if parsed.exit.is_none() {
                parsed.exit = Some(code);
            }
            continue;
        }
        keep.push(trimmed.to_owned());
    }
    if parsed.signal.is_none()
        && let Some(code) = parsed.exit
        && code > 128
    {
        parsed.signal = Some(signal_name(code - 128));
    }
    parsed.diagnostic = diagnostic_line(&keep);
    parsed
}

fn marker_exit(line: &str) -> Option<i32> {
    let rest = line.strip_prefix("[exit code: ")?;
    rest.strip_suffix(']')?.trim().parse().ok()
}

fn marker_signal(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("[killed by signal: ") {
        return Some(rest.strip_suffix(']')?.trim().to_owned());
    }
    line.strip_prefix("Killed by ")
        .map(|name| name.trim().to_owned())
        .filter(|name| name.starts_with("SIG"))
}

fn marker_command_exited(line: &str) -> Option<i32> {
    for prefix in [
        "Command exited with code ",
        "Process exited with code ",
        "Exited with code ",
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn signal_name(signal: i32) -> String {
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return format!("signal {signal}"),
    };
    name.to_owned()
}

fn diagnostic_line(lines: &[String]) -> String {
    let errorish = |line: &str| {
        let lower = line.to_ascii_lowercase();
        [
            "permission denied",
            "denied",
            "error",
            "failed",
            "cannot",
            "could not",
            "no such file",
            "not found",
            "refused",
            "killed",
            "sandbox",
            "bwrap",
            "exception",
            "econn",
            "timed out",
            "timeout",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    };
    lines
        .iter()
        .rev()
        .find(|line| errorish(line))
        .or_else(|| lines.last())
        .map(|line| line.to_owned())
        .unwrap_or_default()
}

fn tool_diagnostic(tool: &str, command: &str, parsed: &ParsedToolOutput) -> String {
    let mut parts = Vec::new();
    if parsed.diagnostic.is_empty() {
        let command = command.trim();
        if !command.is_empty() && command.len() <= 160 {
            parts.push(format!("`{command}`"));
        }
    } else {
        parts.push(parsed.diagnostic.clone());
    }
    if let Some(sandbox) = parsed.sandbox.as_deref() {
        parts.push(format!(
            "the native sandbox refused this command ({sandbox})"
        ));
    }
    if let Some(signal) = parsed.signal.as_deref() {
        parts.push(format!("ended by {signal}"));
    } else if let Some(exit) = parsed.exit {
        parts.push(format!("exit code {exit}"));
    }
    let detail = if parts.is_empty() {
        "the native tool failed".to_owned()
    } else {
        parts.join("; ")
    };
    format!("{tool}: {detail}")
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

/// The outcome-relevant facts in one harness JSON event. Exposed so the live
/// supervisor and the replay corpus parse a line through the same function.
pub(super) fn line_events(program: Program, value: &Value) -> Vec<NativeEvent> {
    events(Some(program), value)
}

fn events(program: Option<Program>, value: &Value) -> Vec<NativeEvent> {
    match program {
        Some(Program::Pi) => pi_events(value),
        Some(Program::Codex) => codex_events(value),
        Some(Program::Dsh) => dsh_events(value),
        Some(Program::Opencode) => opencode_events(value),
        Some(Program::Builtin) | None => Vec::new(),
    }
}

fn text_of(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return (!text.trim().is_empty()).then(|| text.to_owned());
    }
    let content = value.get("content")?.as_array()?;
    let mut text = String::new();
    for item in content {
        if let Some(part) = item.get("text").and_then(Value::as_str) {
            text.push_str(part);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn pi_events(value: &Value) -> Vec<NativeEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_execution_start") => vec![NativeEvent::ToolStarted {
            id: string(value, "toolCallId"),
            tool: value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            args: value
                .get("args")
                .map(ToString::to_string)
                .unwrap_or_default(),
        }],
        Some("tool_execution_end") => {
            let result = value.get("result");
            let output = result
                .and_then(|result| result.get("content"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let exit = result
                .and_then(|result| result.get("structuredContent"))
                .and_then(|content| content.get("exit_code"))
                .and_then(Value::as_i64)
                .map(|code| code as i32)
                .or_else(|| {
                    result
                        .and_then(|result| result.get("isError"))
                        .or_else(|| value.get("isError"))
                        .and_then(Value::as_bool)
                        .filter(|error| *error)
                        .map(|_| 1)
                });
            vec![NativeEvent::ToolFinished {
                id: string(value, "toolCallId"),
                tool: value
                    .get("toolName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                exit,
                signal: None,
                output,
            }]
        }
        Some("message_end") => {
            let message = value.get("message");
            if message
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                != Some("assistant")
            {
                return Vec::new();
            }
            let error = message
                .and_then(|message| message.get("errorMessage"))
                .and_then(Value::as_str)
                .filter(|error| !error.trim().is_empty());
            let stop_reason = message
                .and_then(|message| message.get("stopReason"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let successful = error.is_none() && stop_reason != "error";
            let mut events = Vec::new();
            if let Some(text) = text_of(message) {
                events.push(NativeEvent::Text { text, successful });
            }
            if successful {
                events.push(NativeEvent::AttemptSucceeded);
            } else {
                let detail = error.map(ProviderDetail::from_text).unwrap_or_else(|| {
                    ProviderDetail::message("the native attempt ended with an error")
                });
                events.push(NativeEvent::AttemptFailure {
                    detail,
                    waiting: false,
                });
            }
            events
        }
        Some("auto_retry_end") => {
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                let error = value
                    .get("finalError")
                    .and_then(Value::as_str)
                    .unwrap_or("the native provider failed after its retries");
                vec![NativeEvent::TerminalFailure {
                    detail: ProviderDetail::from_text(error),
                }]
            } else if value.get("success").and_then(Value::as_bool) == Some(true) {
                vec![NativeEvent::TerminalSuccess]
            } else {
                Vec::new()
            }
        }
        Some("agent_settled") => vec![NativeEvent::Settled {
            aborted: value.get("aborted").and_then(Value::as_bool) == Some(true),
        }],
        Some("error") => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the native program reported an error");
            vec![NativeEvent::AttemptFailure {
                detail: ProviderDetail::from_text(message),
                waiting: false,
            }]
        }
        _ => Vec::new(),
    }
}

fn codex_events(value: &Value) -> Vec<NativeEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("item.started") | Some("item.completed") => {
            let Some(item) = value.get("item") else {
                return Vec::new();
            };
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
            let started = value.get("type").and_then(Value::as_str) == Some("item.started");
            match kind {
                "command_execution" => {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    if started {
                        vec![NativeEvent::ToolStarted {
                            id: id.to_owned(),
                            tool: "shell".to_owned(),
                            args: item
                                .get("command")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        }]
                    } else {
                        vec![NativeEvent::ToolFinished {
                            id: id.to_owned(),
                            tool: Some("shell".to_owned()),
                            exit: item
                                .get("exit_code")
                                .and_then(Value::as_i64)
                                .map(|code| code as i32),
                            signal: None,
                            output: item
                                .get("aggregated_output")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        }]
                    }
                }
                "mcp_tool_call" | "mcp_call" => {
                    let id = item
                        .get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let tool = item
                        .get("tool")
                        .or_else(|| item.get("tool_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("forgejo");
                    if started {
                        vec![NativeEvent::ToolStarted {
                            id: id.to_owned(),
                            tool: tool.to_owned(),
                            args: item
                                .get("arguments")
                                .or_else(|| item.get("args"))
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        }]
                    } else {
                        let status = item
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let output = item
                            .get("result")
                            .and_then(|result| result.get("content"))
                            .and_then(Value::as_array)
                            .and_then(|items| items.first())
                            .and_then(|item| item.get("text"))
                            .and_then(Value::as_str)
                            .or_else(|| item.get("output").and_then(Value::as_str))
                            .unwrap_or_default()
                            .to_owned();
                        let error = item
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let exit = error
                            .as_ref()
                            .map(|_| 1)
                            .or_else(|| (status == "failed").then_some(1));
                        vec![NativeEvent::ToolFinished {
                            id: id.to_owned(),
                            tool: Some(tool.to_owned()),
                            exit,
                            signal: None,
                            output: error.unwrap_or(output),
                        }]
                    }
                }
                "agent_message" => item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| {
                        vec![NativeEvent::Text {
                            text: text.to_owned(),
                            successful: false,
                        }]
                    })
                    .unwrap_or_default(),
                "error" => {
                    let Some(text) = item.get("message").and_then(Value::as_str) else {
                        return Vec::new();
                    };
                    if text.starts_with("Model metadata") {
                        return Vec::new();
                    }
                    vec![NativeEvent::AttemptFailure {
                        detail: ProviderDetail::from_text(text),
                        waiting: false,
                    }]
                }
                _ => Vec::new(),
            }
        }
        Some("error") => {
            let Some(message) = value.get("message").and_then(Value::as_str) else {
                return Vec::new();
            };
            let waiting = message.contains("waiting for network");
            vec![NativeEvent::AttemptFailure {
                detail: ProviderDetail::from_text(message),
                waiting,
            }]
        }
        Some("turn.completed") => vec![NativeEvent::TerminalSuccess],
        Some("turn.failed") => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("the native turn failed");
            vec![NativeEvent::TerminalFailure {
                detail: ProviderDetail::from_text(message),
            }]
        }
        _ => Vec::new(),
    }
}

fn dsh_events(value: &Value) -> Vec<NativeEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_call") => vec![NativeEvent::ToolStarted {
            id: string(value, "callId"),
            tool: value
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned(),
            args: value
                .get("input")
                .map(ToString::to_string)
                .unwrap_or_default(),
        }],
        Some("tool_result") => {
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("error");
            vec![NativeEvent::ToolFinished {
                id: string(value, "callId"),
                tool: None,
                exit: (status != "completed").then_some(1),
                signal: None,
                output: value
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }]
        }
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                vec![NativeEvent::Text {
                    text: text.to_owned(),
                    successful: false,
                }]
            })
            .unwrap_or_default(),
        Some("final") => value
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                vec![NativeEvent::Text {
                    text: text.to_owned(),
                    successful: false,
                }]
            })
            .unwrap_or_default(),
        Some("status") => {
            if value.get("phase").and_then(Value::as_str) != Some("turn_end") {
                return Vec::new();
            }
            let reason = value.get("reason");
            let kind = reason
                .and_then(|reason| reason.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            if kind == "completed" {
                return vec![NativeEvent::TerminalSuccess];
            }
            let error = reason.and_then(|reason| reason.get("error"));
            let detail = ProviderDetail {
                status: error
                    .and_then(|error| error.get("status"))
                    .and_then(Value::as_u64)
                    .and_then(|status| u16::try_from(status).ok()),
                kind: Some(kind.to_owned()),
                code: error
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                message: error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the native turn ended with an error")
                    .to_owned(),
                url: None,
            };
            vec![NativeEvent::TerminalFailure { detail }]
        }
        Some("error") => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the native program reported an error");
            vec![NativeEvent::AttemptFailure {
                detail: ProviderDetail::from_text(message),
                waiting: false,
            }]
        }
        _ => Vec::new(),
    }
}

fn opencode_events(value: &Value) -> Vec<NativeEvent> {
    match value.get("type").and_then(Value::as_str) {
        Some("tool_use") => {
            let Some(part) = value.get("part") else {
                return Vec::new();
            };
            let id = string(part, "id");
            let tool = part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_owned();
            let state = part.get("state");
            let status = state
                .and_then(|state| state.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            let args = state
                .and_then(|state| state.get("input"))
                .map(ToString::to_string)
                .unwrap_or_default();
            let mut events = Vec::new();
            if status != "completed" && status != "error" {
                events.push(NativeEvent::ToolStarted { id, tool, args });
                return events;
            }
            let metadata = state
                .and_then(|state| state.get("metadata"))
                .and_then(|metadata| metadata.get("metadata"));
            let exit = metadata
                .and_then(|metadata| metadata.get("exit"))
                .and_then(Value::as_i64)
                .map(|code| code as i32)
                .or_else(|| (status == "error").then_some(1));
            let signal = metadata
                .and_then(|metadata| metadata.get("signal"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut output = state
                .and_then(|state| state.get("output"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(rest) = metadata
                .and_then(|metadata| metadata.get("content"))
                .and_then(Value::as_array)
            {
                for item in rest {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        output.push_str(text);
                        output.push('\n');
                    }
                }
            }
            events.push(NativeEvent::ToolFinished {
                id: id.clone(),
                tool: Some(tool.clone()),
                exit,
                signal,
                output,
            });
            if !events.iter().any(|event| {
                matches!(event, NativeEvent::ToolStarted { id: started, .. } if *started == id)
            }) {
                // The result is the first event for this call: expose the start too,
                // so the transcript has one call object.
                events.insert(
                    0,
                    NativeEvent::ToolStarted {
                        id,
                        tool,
                        args,
                    },
                );
            }
            events
        }
        Some("text") => value
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                vec![NativeEvent::Text {
                    text: text.to_owned(),
                    successful: false,
                }]
            })
            .unwrap_or_default(),
        Some("error") => {
            let error = value.get("error");
            let message = error
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("the native program reported an error");
            let body = error
                .and_then(|error| error.get("response"))
                .and_then(|response| response.get("body"))
                .and_then(Value::as_str);
            let mut detail = ProviderDetail::from_text(
                body.filter(|body| body.contains("error"))
                    .unwrap_or(message),
            );
            if !detail.message.contains(message) && detail.message != message {
                detail.message = message.to_owned();
            }
            detail.kind = error
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(detail.kind);
            detail.status = error
                .and_then(|error| error.get("status"))
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .or(detail.status);
            vec![NativeEvent::AttemptFailure {
                detail,
                waiting: false,
            }]
        }
        _ => Vec::new(),
    }
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi(lines: &[&str]) -> NativeOutcome {
        let mut outcome = NativeOutcome::new(Program::Pi);
        for line in lines {
            outcome.observe_line(line);
        }
        outcome
    }

    #[test]
    fn pi_provider_failure_is_failed_even_though_pi_exits_zero() {
        let mut outcome = pi(&[
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"500: {\"message\":\"synthetic status 500\",\"type\":\"server_error\"}"}}"#,
            r#"{"type":"auto_retry_end","success":false,"attempt":3,"finalError":"500: {\"message\":\"synthetic status 500\",\"type\":\"server_error\"}"}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        let reason = resolution.reason.unwrap();
        assert!(reason.contains("500"), "{reason}");
        assert!(reason.contains("server_error"), "{reason}");
    }

    #[test]
    fn pi_recovered_retry_is_not_failed_and_commits_the_successful_text() {
        let mut outcome = pi(&[
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Connection error."}}"#,
            r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3}"#,
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"probe ok"}]}}"#,
            r#"{"type":"auto_retry_end","success":true,"attempt":1}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ]);
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Succeeded);
        assert_eq!(resolution.text.as_deref(), Some("probe ok"));
    }

    #[test]
    fn failed_attempt_text_is_not_committed() {
        let mut outcome = pi(&[
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Stream ended without finish_reason","content":[{"type":"text","text":"partial"}]}}"#,
            r#"{"type":"auto_retry_end","success":false,"finalError":"Stream ended without finish_reason"}"#,
        ]);
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        assert_eq!(resolution.text, None);
    }

    #[test]
    fn codex_retry_chatter_does_not_fail_a_completed_turn() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        for line in [
            r#"{"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion)"}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"probe ok"}}"#,
            r#"{"type":"turn.completed"}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Succeeded);
        assert_eq!(resolution.text.as_deref(), Some("probe ok"));
    }

    #[test]
    fn codex_terminal_failure_uses_turn_failed_not_retry_chatter() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        for line in [
            r#"{"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion: stream closed before response.completed)"}"#,
            r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion: stream closed before response.completed"}}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(1), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        assert!(resolution.reason.unwrap().contains("stream disconnected"));
    }

    #[test]
    fn dsh_turn_end_is_the_terminal_outcome_not_final() {
        let mut outcome = NativeOutcome::new(Program::Dsh);
        for line in [
            r#"{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"error","error":{"code":"SERVER","message":"500: synthetic status 500"}}}"#,
            r#"{"type":"final","text":""}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(1), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        let reason = resolution.reason.unwrap();
        assert!(reason.contains("SERVER"), "{reason}");
        assert!(reason.contains("500"), "{reason}");
    }

    #[test]
    fn dsh_completed_tool_nonzero_exit_is_a_warning_not_success() {
        let mut outcome = NativeOutcome::new(Program::Dsh);
        for line in [
            r#"{"type":"tool_call","callId":"c1","tool":"bash","input":{"command":"false"}}"#,
            r#"{"type":"tool_result","callId":"c1","status":"completed","result":"[stderr]\n[exit code: 1]"}"#,
            r#"{"type":"status","phase":"turn_end","turn":1,"reason":{"kind":"completed"}}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Succeeded);
        let failure = resolution.first_tool_failure.unwrap();
        assert!(failure.contains("exit code 1"), "{failure}");
    }

    #[test]
    fn opencode_signal_killed_tool_is_a_failure() {
        let mut outcome = NativeOutcome::new(Program::Opencode);
        for line in [
            r#"{"type":"tool_use","part":{"id":"p1","tool":"shell","state":{"status":"completed","metadata":{"metadata":{"signal":"SIGTERM"}},"output":"(no output)"}}}"#,
            r#"{"type":"text","part":{"text":"probe ok"}}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Succeeded);
        let failure = resolution.first_tool_failure.unwrap();
        assert!(failure.contains("SIGTERM"), "{failure}");
    }

    #[test]
    fn a_waiting_harness_at_the_deadline_keeps_its_cause_and_is_not_a_false_failure() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        outcome.observe_line(
            r#"{"type":"error","message":"Reconnecting... waiting for network (Connection failed: error sending request)"}"#,
        );
        let resolution = outcome.finish(None, SupervisorEnd::Deadline);
        assert_eq!(resolution.disposition, Disposition::Failed);
        assert!(resolution.reason.unwrap().contains("Connection failed"));
    }

    #[test]
    fn c5_codex_stderr_retry_names_the_503_while_it_waits() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        assert!(outcome.observe_retry_text(
            "2026-10-26T12:09:15Z INFO codex_otel: event.name=\"codex.api_request\" http.response.status_code=503 error.message=\"http 503\" attempt=0"
        ));
        let (state, cause, terminal, _) = outcome.progress().expect("progress");
        assert_eq!(state, "retrying");
        assert!(cause.contains("503"), "{cause}");
        assert!(!terminal);
        assert!(!outcome.observe_retry_text("compiling one harmless file"));
    }

    #[test]
    fn progress_without_a_failure_cause_is_timed_out() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        outcome.observe_line(r#"{"type":"turn.started"}"#);
        let resolution = outcome.finish(None, SupervisorEnd::Deadline);
        assert_eq!(resolution.disposition, Disposition::TimedOut);
    }
}

#[cfg(test)]
mod timeline_tests {
    use super::*;

    #[test]
    fn a_terminal_provider_failure_supplies_the_reason_over_an_earlier_tool_failure() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        outcome.observe_line(
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"false"}}"#,
        );
        outcome.observe_line(
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","exit_code":1,"status":"failed","aggregated_output":""}}"#,
        );
        outcome.observe_line(
            r#"{"type":"turn.failed","error":{"message":"unexpected status 500: synthetic status 500"}}"#,
        );
        let resolution = outcome.finish(Some(1), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        let reason = resolution.reason.unwrap();
        assert!(reason.contains("500"), "{reason}");
        assert!(!reason.contains("exit code 1"), "{reason}");
        assert!(resolution.first_tool_failure.is_some());
    }

    #[test]
    fn a_terminal_failure_after_a_successful_attempt_still_ends_failed_with_the_terminal_cause() {
        let mut outcome = NativeOutcome::new(Program::Codex);
        outcome.observe_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"the earlier answer"}}"#,
        );
        outcome.observe_line(
            r#"{"type":"turn.failed","error":{"message":"the later provider failure"}}"#,
        );
        let resolution = outcome.finish(Some(1), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Failed);
        assert!(
            resolution
                .reason
                .unwrap()
                .contains("later provider failure")
        );
        assert_eq!(
            resolution.text, None,
            "a candidate from a turn the harness did not complete was committed"
        );
    }
}

#[cfg(test)]
mod attempt_ownership_tests {
    use super::*;

    #[test]
    fn pi_terminal_success_does_not_commit_a_failed_attempts_partial_text() {
        let mut outcome = NativeOutcome::new(Program::Pi);
        for line in [
            r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"Stream ended without finish_reason","content":[{"type":"text","text":"partial"}]}}"#,
            r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3}"#,
            r#"{"type":"auto_retry_end","success":true,"attempt":1}"#,
            r#"{"type":"agent_settled","aborted":false}"#,
        ] {
            outcome.observe_line(line);
        }
        let resolution = outcome.finish(Some(0), SupervisorEnd::Natural);
        assert_eq!(resolution.disposition, Disposition::Succeeded);
        assert_eq!(
            resolution.text, None,
            "text from the failed attempt was committed"
        );
    }

    #[test]
    fn a_failed_tool_then_a_deadline_is_timed_out_with_the_tool_as_a_warning() {
        let mut outcome = NativeOutcome::new(Program::Pi);
        for line in [
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"false"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"(no output)\n\nCommand exited with code 1"}],"structuredContent":{"exit_code":1},"isError":true},"isError":true}"#,
        ] {
            outcome.observe_line(line);
        }
        assert_eq!(outcome.deadline_cause(), None);
        let warning = outcome.progress().and_then(|(_, _, _, warning)| warning);
        assert!(
            warning
                .as_deref()
                .is_some_and(|warning| warning.contains("exit code 1")),
            "the tool warning was not persisted: {warning:?}"
        );
        let resolution = outcome.finish(None, SupervisorEnd::Deadline);
        assert_eq!(
            resolution.disposition,
            Disposition::TimedOut,
            "a tool warning became a session failure"
        );
        assert!(
            resolution
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("time limit"),
            "the deadline reason is not a timeout: {:?}",
            resolution.reason
        );
        assert!(
            resolution
                .first_tool_failure
                .as_deref()
                .unwrap_or_default()
                .contains("exit code 1")
        );
    }
}
