//! The append-only transcript of one run.
//!
//! Every run writes one file, `transcript.jsonl`, one JSON object per line. The file
//! is append-only: nothing already written is ever rewritten, so a transcript read
//! back later is exactly what was written when it happened.
//!
//! Appending is safe against a crash in the middle of a line. A line is written with
//! one append, and a reader treats a final line with no newline after it as
//! *partial* rather than as a record. A writer opening a file that ends in a partial
//! line first writes the missing newline, so a new record is never written onto the
//! remains of an old one.
//!
//! The records are read back as typed values ([`Record`]), so a malformed file is a
//! clear error and never a silent guess.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mode::SECRET_FILE_MODE;
use crate::run::RunOutcome;
use crate::time::{now_unix_ms, rfc3339};

/// The transcript's name inside a run directory.
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// One skill the run had installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    /// The skill's directory name.
    pub name: String,
    /// How many files the skill holds.
    pub files: u32,
    /// How many bytes those files hold.
    pub bytes: u64,
    /// A digest of the skill's `SKILL.md`, so a change is visible.
    pub digest: String,
}

/// One tool call the model asked for, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelToolCall {
    /// The model's own id for the call.
    pub id: String,
    /// The declared tool's name.
    pub name: String,
    /// The arguments, as the model wrote them.
    pub arguments: String,
}

/// The tokens a model reply used, as the endpoint reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Tokens in the request.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens in the reply.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Tokens in both.
    #[serde(default)]
    pub total_tokens: u64,
}

/// One line of a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Record {
    /// The run began.
    RunStarted {
        /// When, in RFC 3339 UTC.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// The agent the run belongs to.
        agent: String,
        /// The agent process's own id.
        pid: u32,
        /// The workspace the process is rooted in.
        workspace: String,
    },
    /// The instruction document was read.
    InstructionRead {
        /// When.
        ts: String,
        /// The document's path.
        path: String,
        /// How many bytes it holds.
        bytes: u64,
        /// A digest of those bytes.
        digest: String,
    },
    /// The skill set was resolved and installed.
    SkillsDiscovered {
        /// When.
        ts: String,
        /// The skills, in name sequence.
        skills: Vec<SkillRef>,
    },
    /// A declared or model-asked tool is about to run.
    ToolCall {
        /// When.
        ts: String,
        /// Which model turn asked for it, or `null` for a declared step.
        #[serde(default)]
        turn: Option<u32>,
        /// Which tool call this is, starting at one.
        index: u32,
        /// The tool's name: `shell`.
        tool: String,
        /// The command handed to the shell.
        command: String,
        /// The directory the shell runs in.
        cwd: String,
    },
    /// A tool has finished.
    ToolResult {
        /// When.
        ts: String,
        /// Which model turn asked for it, or `null` for a declared step.
        #[serde(default)]
        turn: Option<u32>,
        /// Which tool call this is.
        index: u32,
        /// The tool's name.
        tool: String,
        /// The shell's exit code, when it exited.
        exit_code: Option<i32>,
        /// The signal that ended the shell, when a signal did.
        signal: Option<i32>,
        /// How long the tool took, in milliseconds.
        duration_ms: u64,
        /// A bounded, redacted excerpt of what the tool printed.
        #[serde(default)]
        output: Option<String>,
    },
    /// The agent process itself is about to exit.
    ChildExit {
        /// When.
        ts: String,
        /// The process's exit code, when it exited.
        exit_code: Option<i32>,
        /// The signal that ended it, when a signal did.
        signal: Option<i32>,
    },
    /// The bounds the run will be held to, recorded before the first model call.
    RunLimits {
        /// When.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// The declared bound on model turns.
        max_turns: u32,
        /// The declared bound on the run's lifetime, in seconds.
        timeout_secs: u64,
        /// The endpoint the run will call directly.
        endpoint: String,
        /// The model id the run will ask for.
        model: String,
    },
    /// The model call is in flight.
    ///
    /// Written **before** the request goes out, so a run that is waiting on a slow
    /// endpoint is not indistinguishable from a run that has hung: the send time is
    /// on the record while the wait is still happening (W-5).
    ModelRequestSent {
        /// When the request was sent.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// Which turn this is, starting at one.
        turn: u32,
        /// The endpoint called.
        endpoint: String,
        /// The model asked for.
        model: String,
    },
    /// A model request that has been sent, written when the reply arrives.
    ModelRequest {
        /// When.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// Which turn this is, starting at one.
        turn: u32,
        /// The endpoint called.
        endpoint: String,
        /// The model asked for.
        model: String,
        /// The `Authorization` header as recorded: always `Bearer <redacted>`.
        authorization: String,
        /// The request body exactly as sent, with the key redacted.
        body: String,
    },
    /// A model reply arrived.
    ModelReply {
        /// When.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// Which turn this is.
        turn: u32,
        /// The HTTP status the endpoint answered with.
        status: u16,
        /// The text of the reply, when there was any.
        content: Option<String>,
        /// The tool calls the model asked for.
        #[serde(default)]
        tool_calls: Vec<ModelToolCall>,
        /// Why the model stopped, when the endpoint said.
        #[serde(default)]
        finish_reason: Option<String>,
        /// The tokens the endpoint reported.
        #[serde(default)]
        usage: Option<ModelUsage>,
        /// The reply body exactly as it arrived, with the key redacted.
        body: String,
    },
    /// A model exchange failed. The class says what kind of failure it was.
    ModelCallFailed {
        /// When.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// Which turn this was; zero when the run never got as far as a call.
        turn: u32,
        /// The error class: `bad_url`, `missing_key`, `bad_key`, `model_not_found`,
        /// `rate_limited`, `server_error`, `malformed_reply`, `timeout`, `transport`.
        class: String,
        /// The message, which never holds the key.
        message: String,
    },
    /// The run is over. Written by the process that supervised the run, once, last.
    RunFinished {
        /// When.
        ts: String,
        /// The run's identity.
        run_id: String,
        /// One of `succeeded`, `failed`, `stopped`, `timed_out`.
        outcome: String,
        /// The process's exit code, when it exited.
        exit_code: Option<i32>,
        /// How long the run lasted, in milliseconds.
        duration_ms: u64,
        /// Why it ended that way, when there is a reason to give.
        reason: Option<String>,
    },
}

impl Record {
    /// The opening record of a run.
    pub fn run_started(run_id: &str, agent: &str, pid: u32, workspace: &str) -> Self {
        Record::RunStarted {
            ts: stamp(),
            run_id: run_id.to_owned(),
            agent: agent.to_owned(),
            pid,
            workspace: workspace.to_owned(),
        }
    }

    /// The record of an instruction that was read.
    pub fn instruction_read(path: &Path, bytes: &[u8]) -> Self {
        Record::InstructionRead {
            ts: stamp(),
            path: path.display().to_string(),
            bytes: bytes.len() as u64,
            digest: digest(bytes),
        }
    }

    /// The record of a skill set that was installed.
    pub fn skills_discovered(skills: Vec<SkillRef>) -> Self {
        Record::SkillsDiscovered {
            ts: stamp(),
            skills,
        }
    }

    /// The record of a tool about to run. `turn` is `None` for a declared step and
    /// `Some(turn)` when the model asked for it.
    pub fn tool_call(turn: Option<u32>, index: u32, tool: &str, command: &str, cwd: &str) -> Self {
        Record::ToolCall {
            ts: stamp(),
            turn,
            index,
            tool: tool.to_owned(),
            command: command.to_owned(),
            cwd: cwd.to_owned(),
        }
    }

    /// The record of a tool that has finished.
    pub fn tool_result(
        turn: Option<u32>,
        index: u32,
        tool: &str,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
        output: Option<String>,
    ) -> Self {
        Record::ToolResult {
            ts: stamp(),
            turn,
            index,
            tool: tool.to_owned(),
            exit_code,
            signal,
            duration_ms,
            output,
        }
    }

    /// The record of the agent process about to exit.
    pub fn child_exit(exit_code: Option<i32>, signal: Option<i32>) -> Self {
        Record::ChildExit {
            ts: stamp(),
            exit_code,
            signal,
        }
    }

    /// The bounds the run is held to, recorded before the first model call.
    pub fn run_limits(
        run_id: &str,
        max_turns: u32,
        timeout_secs: u64,
        endpoint: &str,
        model: &str,
    ) -> Self {
        Record::RunLimits {
            ts: stamp(),
            run_id: run_id.to_owned(),
            max_turns,
            timeout_secs,
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
        }
    }

    /// The record of a model request about to be sent.
    pub fn model_request(
        run_id: &str,
        turn: u32,
        endpoint: &str,
        model: &str,
        authorization: &str,
        body: &str,
    ) -> Self {
        Record::ModelRequest {
            ts: stamp(),
            run_id: run_id.to_owned(),
            turn,
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            authorization: authorization.to_owned(),
            body: body.to_owned(),
        }
    }

    /// The record that a model call is in flight, written before it is sent.
    pub fn model_request_sent(run_id: &str, turn: u32, endpoint: &str, model: &str) -> Self {
        Record::ModelRequestSent {
            ts: stamp(),
            run_id: run_id.to_owned(),
            turn,
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
        }
    }

    /// The record of a model reply that arrived.
    #[allow(clippy::too_many_arguments)]
    pub fn model_reply(
        run_id: &str,
        turn: u32,
        status: u16,
        content: Option<String>,
        tool_calls: Vec<ModelToolCall>,
        finish_reason: Option<String>,
        usage: Option<ModelUsage>,
        body: &str,
    ) -> Self {
        Record::ModelReply {
            ts: stamp(),
            run_id: run_id.to_owned(),
            turn,
            status,
            content,
            tool_calls,
            finish_reason,
            usage,
            body: body.to_owned(),
        }
    }

    /// The record of a model exchange that failed, with its class.
    pub fn model_call_failed(run_id: &str, turn: u32, class: &str, message: &str) -> Self {
        Record::ModelCallFailed {
            ts: stamp(),
            run_id: run_id.to_owned(),
            turn,
            class: class.to_owned(),
            message: message.to_owned(),
        }
    }

    /// The terminal record of a run.
    pub fn run_finished(
        run_id: &str,
        outcome: RunOutcome,
        exit_code: Option<i32>,
        duration_ms: u64,
        reason: Option<String>,
    ) -> Self {
        Record::RunFinished {
            ts: stamp(),
            run_id: run_id.to_owned(),
            outcome: outcome.as_str().to_owned(),
            exit_code,
            duration_ms,
            reason,
        }
    }

    /// The record's tag, as it is written.
    pub fn event(&self) -> &'static str {
        match self {
            Record::RunStarted { .. } => "run_started",
            Record::InstructionRead { .. } => "instruction_read",
            Record::SkillsDiscovered { .. } => "skills_discovered",
            Record::ToolCall { .. } => "tool_call",
            Record::ToolResult { .. } => "tool_result",
            Record::ChildExit { .. } => "child_exit",
            Record::RunLimits { .. } => "run_limits",
            Record::ModelRequestSent { .. } => "model_request_sent",
            Record::ModelRequest { .. } => "model_request",
            Record::ModelReply { .. } => "model_reply",
            Record::ModelCallFailed { .. } => "model_call_failed",
            Record::RunFinished { .. } => "run_finished",
        }
    }

    /// When the record was written.
    pub fn timestamp(&self) -> &str {
        match self {
            Record::RunStarted { ts, .. }
            | Record::InstructionRead { ts, .. }
            | Record::SkillsDiscovered { ts, .. }
            | Record::ToolCall { ts, .. }
            | Record::ToolResult { ts, .. }
            | Record::ChildExit { ts, .. }
            | Record::RunLimits { ts, .. }
            | Record::ModelRequestSent { ts, .. }
            | Record::ModelRequest { ts, .. }
            | Record::ModelReply { ts, .. }
            | Record::ModelCallFailed { ts, .. }
            | Record::RunFinished { ts, .. } => ts,
        }
    }
}

/// The one tool this round declares.
pub const SHELL_TOOL: &str = "shell";

/// An open transcript, ready to append.
#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    file: fs::File,
}

impl Transcript {
    /// Open `run_dir/transcript.jsonl` for appending, creating it if it is absent.
    ///
    /// A file whose last line has no newline after it is the remains of a crash: that
    /// final, incomplete line is dropped before anything is appended, so a new record
    /// is never written onto it. Losing an incomplete line is losing a record that was
    /// never finished.
    pub fn open(run_dir: &Path) -> io::Result<Self> {
        let path = run_dir.join(TRANSCRIPT_FILE);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(SECRET_FILE_MODE)
            .open(&path)?;
        drop_incomplete_tail(&mut file)?;
        Ok(Self { path, file })
    }

    /// The file this transcript writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record as one line.
    pub fn append(&mut self, record: &Record) -> io::Result<()> {
        let line = serde_json::to_string(record).map_err(io::Error::other)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

/// A transcript read back from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    /// Every complete record, in the order it was written.
    pub records: Vec<Record>,
    /// The text of a final line with no newline after it, if there was one.
    pub partial: Option<String>,
}

/// Read a transcript back. A partial final line is reported, not parsed.
pub fn read(run_dir: &Path) -> io::Result<TranscriptFile> {
    let path = run_dir.join(TRANSCRIPT_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(TranscriptFile {
                records: Vec::new(),
                partial: None,
            });
        }
        Err(source) => return Err(source),
    };
    let mut records = Vec::new();
    let mut partial = None;
    for line in text.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.is_empty() {
            continue;
        }
        if complete {
            let record: Record = serde_json::from_str(body).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{path:?} has a line that is not a record: {error}"),
                )
            })?;
            records.push(record);
        } else {
            partial = Some(body.to_owned());
        }
    }
    Ok(TranscriptFile { records, partial })
}

/// The run's terminal record, when it has one.
pub fn terminal_record(run_dir: &Path) -> Option<Record> {
    let file = read(run_dir).ok()?;
    file.records
        .into_iter()
        .rev()
        .find(|record| matches!(record, Record::RunFinished { .. }))
}

/// The timestamp of the run's opening record, when it is there.
pub fn first_started_at(run_dir: &Path) -> Option<String> {
    let file = read(run_dir).ok()?;
    file.records
        .iter()
        .find(|record| matches!(record, Record::RunStarted { .. }))
        .map(|record| record.timestamp().to_owned())
}

/// The process id of the run's opening record, when it is there.
///
/// A reopened UI uses this to tell a run still in progress from one whose process is
/// gone: the run's own record says which process it was.
pub fn first_started_pid(run_dir: &Path) -> Option<u32> {
    let file = read(run_dir).ok()?;
    file.records.iter().find_map(|record| match record {
        Record::RunStarted { pid, .. } => Some(*pid),
        _ => None,
    })
}

/// A short, stable digest of some bytes: FNV-1a, 64-bit, written as hex.
///
/// This is not a cryptographic hash and is not used as one. It exists so that "the
/// instruction the run read" and "the skill the run installed" can be compared with
/// a later read, which is all a transcript needs.
pub fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

/// The current instant as a transcript timestamp.
fn stamp() -> String {
    rfc3339(now_unix_ms())
}

/// Drop a final line that has no newline after it.
fn drop_incomplete_tail(file: &mut fs::File) -> io::Result<()> {
    let length = file.seek(SeekFrom::End(0))?;
    if length == 0 {
        return Ok(());
    }
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut bytes)?;
    if bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    let keep = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => at as u64 + 1,
        None => 0,
    };
    file.set_len(keep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_root;

    fn run_dir(tag: &str) -> PathBuf {
        let dir = scratch_root(tag).join("run-test");
        fs::create_dir_all(&dir).expect("the run directory is created");
        dir
    }

    #[test]
    fn every_record_kind_survives_a_round_trip() {
        let dir = run_dir("transcript-round-trip");
        let records = vec![
            Record::run_started("run-1", "scribe", 1234, "/tmp/ws"),
            Record::instruction_read(Path::new("/tmp/i.md"), b"do the thing"),
            Record::skills_discovered(vec![SkillRef {
                name: "alpha".to_owned(),
                files: 1,
                bytes: 12,
                digest: digest(b"# alpha"),
            }]),
            Record::tool_call(None, 1, SHELL_TOOL, "echo one", "/tmp/ws"),
            Record::tool_result(
                None,
                1,
                SHELL_TOOL,
                Some(0),
                None,
                4,
                Some("one\n".to_owned()),
            ),
            Record::run_limits("run-1", 4, 30, "http://127.0.0.1:1/v1", "m"),
            Record::model_request(
                "run-1",
                1,
                "http://127.0.0.1:1/v1",
                "m",
                "Bearer <redacted>",
                "{\"model\":\"m\"}",
            ),
            Record::model_reply(
                "run-1",
                1,
                200,
                Some("done".to_owned()),
                vec![ModelToolCall {
                    id: "call_1".to_owned(),
                    name: SHELL_TOOL.to_owned(),
                    arguments: "{\"command\":\"ls\"}".to_owned(),
                }],
                Some("stop".to_owned()),
                Some(ModelUsage {
                    prompt_tokens: 5,
                    completion_tokens: 2,
                    total_tokens: 7,
                }),
                "{\"choices\":[]}",
            ),
            Record::model_call_failed("run-1", 2, "bad_key", "the endpoint refused the key"),
            Record::child_exit(Some(0), None),
            Record::run_finished("run-1", RunOutcome::Succeeded, Some(0), 42, None),
        ];
        {
            let mut transcript = Transcript::open(&dir).expect("opened");
            for record in &records {
                transcript.append(record).expect("appended");
            }
        }
        let back = read(&dir).expect("read back");
        assert_eq!(back.records, records);
        assert_eq!(back.partial, None);
    }

    #[test]
    fn every_record_is_written_as_one_line_with_an_event_tag() {
        let dir = run_dir("transcript-tags");
        let mut transcript = Transcript::open(&dir).expect("opened");
        transcript
            .append(&Record::run_started("run-1", "a", 1, "/w"))
            .expect("appended");
        transcript
            .append(&Record::run_finished(
                "run-1",
                RunOutcome::Failed,
                Some(1),
                5,
                None,
            ))
            .expect("appended");
        let text = fs::read_to_string(dir.join(TRANSCRIPT_FILE)).expect("readable");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert!(value.get("event").is_some(), "{line}");
            assert!(!line.contains('\n'));
        }
        assert!(lines[0].contains("\"event\":\"run_started\""));
        assert!(lines[1].contains("\"event\":\"run_finished\""));
    }

    #[test]
    fn a_partial_final_line_is_reported_and_not_parsed() {
        let dir = run_dir("transcript-partial");
        let path = dir.join(TRANSCRIPT_FILE);
        let mut transcript = Transcript::open(&dir).expect("opened");
        transcript
            .append(&Record::run_started("run-1", "a", 1, "/w"))
            .expect("appended");
        drop(transcript);
        // Simulate a crash in the middle of a line.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(b"{\"event\":\"tool_ca").expect("written");
        drop(file);

        let back = read(&dir).expect("read back");
        assert_eq!(back.records.len(), 1);
        assert_eq!(back.partial.as_deref(), Some("{\"event\":\"tool_ca"));
    }

    #[test]
    fn a_partial_line_left_by_a_crash_is_dropped_before_the_next_record() {
        let dir = run_dir("transcript-repair");
        let path = dir.join(TRANSCRIPT_FILE);
        let mut transcript = Transcript::open(&dir).expect("opened");
        transcript
            .append(&Record::run_started("run-1", "a", 1, "/w"))
            .expect("appended");
        drop(transcript);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open")
            .write_all(b"{\"event\":\"half")
            .expect("written");

        let mut transcript = Transcript::open(&dir).expect("opened again");
        transcript
            .append(&Record::run_finished(
                "run-1",
                RunOutcome::Succeeded,
                Some(0),
                1,
                None,
            ))
            .expect("appended");
        let back = read(&dir).expect("read back");
        assert_eq!(back.records.len(), 2, "the incomplete line is gone");
        assert_eq!(back.partial, None);
        assert!(matches!(back.records[1], Record::RunFinished { .. }));
        let text = fs::read_to_string(&path).expect("readable");
        assert!(!text.contains("half"), "the remains are not kept: {text}");
    }

    #[test]
    fn a_line_that_is_not_a_record_is_an_error_rather_than_a_guess() {
        let dir = run_dir("transcript-corrupt");
        fs::write(dir.join(TRANSCRIPT_FILE), "not json at all\n").expect("written");
        assert!(read(&dir).is_err());
    }

    #[test]
    fn an_unknown_event_is_refused() {
        let dir = run_dir("transcript-unknown-event");
        fs::write(
            dir.join(TRANSCRIPT_FILE),
            "{\"event\":\"something_else\",\"ts\":\"now\"}\n",
        )
        .expect("written");
        assert!(read(&dir).is_err());
    }

    #[test]
    fn the_terminal_record_is_the_last_run_finished() {
        let dir = run_dir("transcript-terminal");
        let mut transcript = Transcript::open(&dir).expect("opened");
        transcript
            .append(&Record::run_started("run-1", "a", 1, "/w"))
            .expect("appended");
        transcript
            .append(&Record::run_finished(
                "run-1",
                RunOutcome::TimedOut,
                None,
                900,
                Some("the run reached its timeout".to_owned()),
            ))
            .expect("appended");
        let terminal = terminal_record(&dir).expect("a terminal record");
        assert!(matches!(
            terminal,
            Record::RunFinished { ref outcome, .. } if outcome == "timed_out"
        ));
    }

    #[test]
    fn a_run_with_no_terminal_record_has_none() {
        let dir = run_dir("transcript-no-terminal");
        let mut transcript = Transcript::open(&dir).expect("opened");
        transcript
            .append(&Record::child_exit(Some(0), None))
            .expect("appended");
        assert!(terminal_record(&dir).is_none());
    }

    #[test]
    fn a_digest_changes_with_the_content_and_is_stable_for_the_same_content() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert!(digest(b"abc").starts_with("fnv1a64:"));
    }

    #[test]
    fn a_missing_transcript_reads_as_empty_rather_than_failing() {
        let dir = scratch_root("transcript-missing").join("nothing");
        let file = read(&dir).expect("no file is an empty transcript");
        assert!(file.records.is_empty());
        assert!(file.partial.is_none());
    }
}
