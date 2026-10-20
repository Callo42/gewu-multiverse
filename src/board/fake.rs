//! A scripted engine for the board's demonstrations and tests.
//!
//! It implements the real [`EngineApi`] contract, but every slow thing is a counter
//! advanced by [`EngineApi::tick`]: fetching Forgejo a few percent at a time, one
//! account per tick, sessions that end and start again. The script is deliberately
//! legible; the board cannot tell this engine from the real one.

use crate::engine_api::{
    AgentView, BoardView, Command, Desired, EndpointTest, EndpointView, EngineApi, ForgejoState,
    ForgejoView, Provisioning, SessionEnd, SessionState,
};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const FETCH_STEP: u8 = 7;
const PREPARE_TICKS: u8 = 4;
const STARTING_TICKS: u8 = 4;
const SESSION_TICKS: u32 = 8;
const RESTART_TICKS: u32 = 4;
const MAX_AGENTS: usize = 20;

/// The terms the fake first run shows, one short item each. The real engine must
/// supply exactly these five sentences.
const TERMS: [&str; 5] = [
    "Agents use this computer with your permissions.",
    "They can read your files, including saved keys.",
    "Each has its own login on your Forgejo.",
    "Started agents keep going until you stop them.",
    "Their work is sent to your model provider.",
];

/// A fake Forgejo, model endpoint and set of agents.
pub struct FakeEngine {
    terms_acknowledged: bool,
    forgejo: ForgejoState,
    phase_ticks: u8,
    repositories: usize,
    agents: Vec<FakeAgent>,
    endpoint_url: String,
    endpoint_model: String,
    endpoint_key_present: bool,
    test: EndpointTest,
    test_pending: bool,
    notice: Option<String>,
    reveal_pending: bool,
    admin_password: String,
    instruction_root: PathBuf,
    reveal_sent: bool,
    shutdown_called: bool,
}

struct FakeAgent {
    name: String,
    login: String,
    provisioning: Provisioning,
    desired: Desired,
    session: SessionState,
    sessions_since_start: u64,
    recent: Vec<SessionEnd>,
    instruction_path: PathBuf,
    instruction_chars: usize,
    instruction_first_line: Option<String>,
    activity: Option<String>,
    session_ticks: u32,
    restart_ticks: u32,
}

/// The one source of unique scratch roots in this process.
static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl FakeEngine {
    /// A fresh fake: Forgejo missing, no agents, no endpoint, terms unread.
    pub fn new() -> Self {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("gewu-board-fake-{}-{sequence}", std::process::id()));
        let _ = fs::DirBuilder::new().mode(0o700).create(&root);
        Self {
            terms_acknowledged: false,
            forgejo: ForgejoState::Missing,
            phase_ticks: 0,
            repositories: 0,
            agents: Vec::new(),
            endpoint_url: String::new(),
            endpoint_model: String::new(),
            endpoint_key_present: false,
            test: EndpointTest::NotRun,
            test_pending: false,
            notice: None,
            reveal_pending: false,
            admin_password: "demo-admin-pw".to_owned(),
            instruction_root: root,
            reveal_sent: false,
            shutdown_called: false,
        }
    }

    /// Whether [`EngineApi::shutdown`] has been called.
    pub fn shutdown_called(&self) -> bool {
        self.shutdown_called
    }

    /// The private scratch root holding the fake instruction and session files.
    pub fn scratch_root(&self) -> &Path {
        &self.instruction_root
    }

    /// The current Forgejo state, for tests that check the script itself.
    pub fn forgejo_state(&self) -> ForgejoState {
        self.forgejo.clone()
    }

    /// Move Forgejo forward one tick. Called by [`EngineApi::tick`].
    fn advance_forgejo(&mut self) {
        match self.forgejo {
            ForgejoState::Fetching { percent } => {
                let next = percent.unwrap_or(0).saturating_add(FETCH_STEP);
                if next >= 100 {
                    self.forgejo = ForgejoState::Preparing;
                    self.phase_ticks = 0;
                } else {
                    self.forgejo = ForgejoState::Fetching {
                        percent: Some(next),
                    };
                }
            }
            ForgejoState::Preparing => {
                self.phase_ticks += 1;
                if self.phase_ticks >= PREPARE_TICKS {
                    self.forgejo = ForgejoState::Starting;
                    self.phase_ticks = 0;
                }
            }
            ForgejoState::Starting => {
                self.phase_ticks += 1;
                if self.phase_ticks >= STARTING_TICKS {
                    self.forgejo = ForgejoState::Running;
                    self.repositories = 2;
                }
            }
            ForgejoState::Missing
            | ForgejoState::Running
            | ForgejoState::Stopped
            | ForgejoState::Failed { .. } => {}
        }
    }

    /// Make one pending account ready, at most once per tick.
    fn provision_one(&mut self) {
        if self.forgejo != ForgejoState::Running {
            return;
        }
        if let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.provisioning == Provisioning::Pending)
        {
            agent.provisioning = Provisioning::Ready;
        }
    }

    /// Read an instruction file again, so writing it is what unblocks an agent.
    fn refresh_instruction(agent: &mut FakeAgent) {
        let text = fs::read_to_string(&agent.instruction_path).unwrap_or_default();
        agent.instruction_chars = text.chars().count();
        agent.instruction_first_line = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned);
    }

    fn begin_session(agent: &mut FakeAgent) {
        agent.sessions_since_start += 1;
        agent.session = SessionState::Running {
            number: agent.sessions_since_start,
            since: SystemTime::now(),
        };
        agent.session_ticks = 0;
        agent.restart_ticks = 0;
        agent.activity = match agent.sessions_since_start % 3 {
            1 => Some(format!("commented on #{}", agent.sessions_since_start)),
            2 => Some(format!(
                "opened pull request #{}",
                agent.sessions_since_start
            )),
            _ => None,
        };
    }

    fn end_session(agent: &mut FakeAgent) {
        let end = if agent.sessions_since_start % 3 == 0 {
            SessionEnd::Failed {
                reason: "model error".to_owned(),
            }
        } else {
            SessionEnd::Succeeded
        };
        agent.recent.push(end.clone());
        if agent.recent.len() > 10 {
            agent.recent.remove(0);
        }
        agent.session = SessionState::Restarting {
            next_at: SystemTime::now() + Duration::from_secs(1),
            last: end,
        };
        agent.restart_ticks = RESTART_TICKS;
        agent.activity = None;
    }

    fn stop_agent(agent: &mut FakeAgent) {
        if matches!(agent.session, SessionState::Running { .. }) {
            agent.recent.push(SessionEnd::Stopped);
            if agent.recent.len() > 10 {
                agent.recent.remove(0);
            }
        }
        agent.desired = Desired::Stopped;
        agent.session = SessionState::Idle;
        agent.activity = None;
        agent.session_ticks = 0;
        agent.restart_ticks = 0;
    }

    fn start_agent(&mut self, name: &str) -> Result<(), String> {
        let forgejo_running = self.forgejo == ForgejoState::Running;
        let model_ready =
            !self.endpoint_url.trim().is_empty() && !self.endpoint_model.trim().is_empty();
        let Some(agent) = self.agents.iter_mut().find(|agent| agent.name == name) else {
            return Err(format!("no agent named {name}"));
        };
        Self::refresh_instruction(agent);
        agent.desired = Desired::Started;
        if agent.instruction_chars == 0 {
            agent.session = SessionState::Blocked {
                reason: "no instruction".to_owned(),
            };
            agent.activity = None;
        } else if !model_ready {
            agent.session = SessionState::Blocked {
                reason: "no model".to_owned(),
            };
            agent.activity = None;
        } else if !forgejo_running {
            agent.session = SessionState::Blocked {
                reason: "Forgejo not running".to_owned(),
            };
            agent.activity = None;
        } else if agent.provisioning == Provisioning::Ready {
            Self::begin_session(agent);
        } else {
            agent.session = SessionState::Idle;
        }
        Ok(())
    }

    fn tick_agent(&mut self, index: usize) {
        // Refresh the instruction before deciding, so an edit unblocks at once.
        Self::refresh_instruction(&mut self.agents[index]);
        let forgejo_running = self.forgejo == ForgejoState::Running;
        let model_ready =
            !self.endpoint_url.trim().is_empty() && !self.endpoint_model.trim().is_empty();
        let agent = &mut self.agents[index];
        if agent.provisioning != Provisioning::Ready {
            agent.session = SessionState::Idle;
            return;
        }
        if agent.desired == Desired::Stopped {
            if matches!(agent.session, SessionState::Running { .. }) {
                agent.recent.push(SessionEnd::Stopped);
                if agent.recent.len() > 10 {
                    agent.recent.remove(0);
                }
            }
            agent.session = SessionState::Idle;
            agent.activity = None;
            return;
        }
        if agent.instruction_chars == 0 {
            agent.session = SessionState::Blocked {
                reason: "no instruction".to_owned(),
            };
            agent.activity = None;
            return;
        }
        if !model_ready {
            agent.session = SessionState::Blocked {
                reason: "no model".to_owned(),
            };
            agent.activity = None;
            return;
        }
        if !forgejo_running {
            agent.session = SessionState::Blocked {
                reason: "Forgejo not running".to_owned(),
            };
            agent.activity = None;
            return;
        }
        match agent.session {
            SessionState::Running { .. } => {
                agent.session_ticks += 1;
                if agent.session_ticks >= SESSION_TICKS {
                    Self::end_session(agent);
                }
            }
            SessionState::Restarting { .. } => {
                if agent.restart_ticks > 0 {
                    agent.restart_ticks -= 1;
                } else {
                    Self::begin_session(agent);
                }
            }
            SessionState::Idle | SessionState::Blocked { .. } => Self::begin_session(agent),
        }
    }

    fn new_agent(&mut self, number: usize) -> FakeAgent {
        let name = format!("agent-{number}");
        let login = format!("gewu-agent-{number}");
        let dir = self.instruction_root.join(&name);
        let _ = fs::create_dir_all(&dir);
        let instruction_path = dir.join("instruction.md");
        let _ = fs::write(&instruction_path, "");
        FakeAgent {
            name,
            login,
            provisioning: Provisioning::Pending,
            desired: Desired::Stopped,
            session: SessionState::Idle,
            sessions_since_start: 0,
            recent: Vec::new(),
            instruction_path,
            instruction_chars: 0,
            instruction_first_line: None,
            activity: None,
            session_ticks: 0,
            restart_ticks: 0,
        }
    }

    fn next_number(&self) -> usize {
        self.agents
            .iter()
            .filter_map(|agent| agent.name.strip_prefix("agent-"))
            .filter_map(|number| number.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }

    fn start_fetching(&mut self) {
        match self.forgejo {
            ForgejoState::Missing | ForgejoState::Stopped | ForgejoState::Failed { .. } => {
                self.forgejo = ForgejoState::Fetching { percent: Some(0) };
                self.phase_ticks = 0;
            }
            _ => {}
        }
    }

    fn session_paths(&self, name: &str) -> Option<(PathBuf, PathBuf)> {
        if !self.agents.iter().any(|agent| agent.name == name) {
            return None;
        }
        let dir = self.instruction_root.join(name);
        let _ = fs::create_dir_all(&dir);
        let log = dir.join("run.log");
        let transcript = dir.join("transcript.jsonl");
        if !log.is_file() {
            let _ = fs::write(
                &log,
                "demo run\nstep 1  working\nstep 2  commented on #12\n",
            );
        }
        if !transcript.is_file() {
            let _ = fs::write(
                &transcript,
                "{\"turn\":1,\"text\":\"demo run\"}\n{\"turn\":2,\"text\":\"commented on #12\"}\n",
            );
        }
        Some((log, transcript))
    }
}

impl Default for FakeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.instruction_root);
    }
}

impl EngineApi for FakeEngine {
    fn board(&mut self) -> BoardView {
        let admin_password = if self.reveal_pending {
            self.reveal_pending = false;
            self.reveal_sent = true;
            Some(self.admin_password.clone())
        } else {
            None
        };
        let agents = self
            .agents
            .iter()
            .map(|agent| AgentView {
                name: agent.name.clone(),
                login: agent.login.clone(),
                provisioning: agent.provisioning.clone(),
                desired: agent.desired,
                session: agent.session.clone(),
                sessions_since_start: agent.sessions_since_start,
                recent: agent.recent.clone(),
                instruction_chars: agent.instruction_chars,
                instruction_first_line: agent.instruction_first_line.clone(),
                activity: agent.activity.clone(),
            })
            .collect();
        BoardView {
            terms_acknowledged: self.terms_acknowledged,
            terms: TERMS.iter().map(|item| (*item).to_owned()).collect(),
            setup_complete: self.terms_acknowledged
                && !self.endpoint_url.trim().is_empty()
                && !self.endpoint_model.trim().is_empty()
                && !self.agents.is_empty(),
            forgejo: ForgejoView {
                state: self.forgejo.clone(),
                url: "http://127.0.0.1:3000".to_owned(),
                version: "17.0.0".to_owned(),
                admin_login: "admin".to_owned(),
                organisation: "team".to_owned(),
                repositories: self.repositories,
            },
            agents,
            endpoint: EndpointView {
                configured: !self.endpoint_url.trim().is_empty()
                    && !self.endpoint_model.trim().is_empty(),
                url: self.endpoint_url.clone(),
                model: self.endpoint_model.clone(),
                key_present: self.endpoint_key_present,
                test: self.test.clone(),
            },
            notice: self.notice.take(),
            admin_password,
        }
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::AcknowledgeTerms => {
                self.terms_acknowledged = true;
                self.notice = Some("terms recorded".to_owned());
            }
            Command::SetEndpoint { url, model, key } => {
                self.endpoint_url = url;
                self.endpoint_model = model;
                if let Some(key) = key {
                    if !key.is_empty() {
                        self.endpoint_key_present = true;
                    }
                }
                self.test = EndpointTest::NotRun;
                self.test_pending = false;
            }
            Command::TestEndpoint => {
                self.test = EndpointTest::Running;
                self.test_pending = true;
            }
            Command::CreateAgents { count } => {
                if !self.agents.is_empty() {
                    return Err("agents already exist".to_owned());
                }
                if !(1..=12).contains(&count) {
                    return Err("agents must be 1 to 12".to_owned());
                }
                for number in 1..=count {
                    let agent = self.new_agent(number);
                    self.agents.push(agent);
                }
                self.start_fetching();
                self.notice = Some(format!("creating {count} agents"));
            }
            Command::AddAgent => {
                if self.agents.len() >= MAX_AGENTS {
                    return Err(format!("{MAX_AGENTS} agents is the most"));
                }
                let number = self.next_number();
                let agent = self.new_agent(number);
                self.notice = Some(format!("agent-{number} added"));
                self.agents.push(agent);
            }
            Command::RemoveAgent { name } => {
                let Some(index) = self.agents.iter().position(|agent| agent.name == name) else {
                    return Err(format!("no agent named {name}"));
                };
                self.agents.remove(index);
                self.notice = Some(format!("{name} removed"));
            }
            Command::Start { name } => self.start_agent(&name)?,
            Command::Stop { name } => {
                let Some(agent) = self.agents.iter_mut().find(|agent| agent.name == name) else {
                    return Err(format!("no agent named {name}"));
                };
                Self::stop_agent(agent);
            }
            Command::StartForgejo => self.start_fetching(),
            Command::RevealAdminPassword => self.reveal_pending = true,
        }
        Ok(())
    }

    fn tick(&mut self) {
        if self.shutdown_called {
            return;
        }
        self.advance_forgejo();
        self.provision_one();
        for index in 0..self.agents.len() {
            self.tick_agent(index);
        }
        if self.test_pending {
            self.test_pending = false;
            self.test = if self.endpoint_url.contains("/chat/completions") {
                EndpointTest::Passed {
                    answer: "pong".to_owned(),
                }
            } else {
                EndpointTest::Failed {
                    reason: "add /chat/completions".to_owned(),
                }
            };
        }
    }

    fn instruction_path(&self, agent: &str) -> Option<PathBuf> {
        self.agents
            .iter()
            .find(|candidate| candidate.name == agent)
            .map(|candidate| candidate.instruction_path.clone())
    }

    fn session_files(&self, agent: &str) -> Option<(PathBuf, PathBuf)> {
        self.session_paths(agent)
    }

    fn shutdown(&mut self, _grace: Duration) {
        if self.shutdown_called {
            return;
        }
        self.shutdown_called = true;
        self.forgejo = ForgejoState::Stopped;
        self.repositories = 0;
        self.test_pending = false;
        for agent in &mut self.agents {
            agent.desired = Desired::Stopped;
            agent.session = SessionState::Idle;
            agent.activity = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_engine(count: usize) -> FakeEngine {
        let mut engine = FakeEngine::new();
        engine.send(Command::AcknowledgeTerms).expect("terms");
        engine
            .send(Command::SetEndpoint {
                url: "https://model.example/v1/chat/completions".to_owned(),
                model: "demo".to_owned(),
                key: Some("demo-key".to_owned()),
            })
            .expect("endpoint");
        engine
            .send(Command::CreateAgents { count })
            .expect("create");
        engine.send(Command::StartForgejo).expect("start forgejo");
        for _ in 0..200 {
            engine.tick();
        }
        engine
    }

    #[test]
    fn the_fake_supplies_the_exact_five_terms() {
        let mut engine = FakeEngine::new();
        let view = engine.board();
        assert_eq!(
            view.terms,
            [
                "Agents use this computer with your permissions.",
                "They can read your files, including saved keys.",
                "Each has its own login on your Forgejo.",
                "Started agents keep going until you stop them.",
                "Their work is sent to your model provider.",
            ]
        );
    }

    #[test]
    fn an_agent_with_no_model_is_blocked_until_it_has_one() {
        let mut engine = FakeEngine::new();
        engine.send(Command::AcknowledgeTerms).expect("terms");
        engine
            .send(Command::CreateAgents { count: 1 })
            .expect("create");
        engine.send(Command::StartForgejo).expect("forgejo");
        for _ in 0..40 {
            engine.tick();
        }
        let path = engine.instruction_path("agent-1").expect("path");
        fs::write(&path, "work\n").expect("instruction");
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        let board = engine.board();
        assert!(matches!(
            board.agents[0].session,
            SessionState::Blocked { ref reason } if reason == "no model"
        ));
    }

    #[test]
    fn forgejo_fetches_in_steps_prepares_starts_and_runs() {
        let mut engine = FakeEngine::new();
        assert_eq!(engine.forgejo_state(), ForgejoState::Missing);
        engine.send(Command::StartForgejo).expect("start");
        let mut seen = Vec::new();
        for _ in 0..40 {
            engine.tick();
            let state = engine.forgejo_state();
            if seen.last() != Some(&state) {
                seen.push(state.clone());
            }
            if state == ForgejoState::Running {
                break;
            }
        }
        assert!(matches!(seen[0], ForgejoState::Fetching { .. }));
        assert!(
            seen.iter()
                .any(|state| matches!(state, ForgejoState::Preparing))
        );
        assert!(
            seen.iter()
                .any(|state| matches!(state, ForgejoState::Starting))
        );
        assert_eq!(engine.forgejo_state(), ForgejoState::Running);
    }

    #[test]
    fn accounts_come_ready_one_per_tick() {
        let mut engine = running_engine(3);
        let board = engine.board();
        assert!(
            board
                .agents
                .iter()
                .all(|agent| agent.provisioning == Provisioning::Ready)
        );
    }

    #[test]
    fn an_endpoint_without_chat_completions_fails_and_one_with_it_passes() {
        let mut engine = FakeEngine::new();
        engine
            .send(Command::SetEndpoint {
                url: "https://model.example/v1".to_owned(),
                model: "demo".to_owned(),
                key: None,
            })
            .expect("set");
        engine.send(Command::TestEndpoint).expect("test");
        engine.tick();
        assert!(matches!(
            engine.board().endpoint.test,
            EndpointTest::Failed { .. }
        ));
        engine
            .send(Command::SetEndpoint {
                url: "https://model.example/v1/chat/completions".to_owned(),
                model: "demo".to_owned(),
                key: None,
            })
            .expect("set");
        engine.send(Command::TestEndpoint).expect("test");
        engine.tick();
        assert!(matches!(
            engine.board().endpoint.test,
            EndpointTest::Passed { .. }
        ));
    }

    #[test]
    fn a_started_agent_without_an_instruction_is_blocked_until_it_has_one() {
        let mut engine = running_engine(1);
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        let board = engine.board();
        assert!(matches!(
            board.agents[0].session,
            SessionState::Blocked { .. }
        ));
        let path = engine.instruction_path("agent-1").expect("path");
        fs::write(&path, "fix the login test\n").expect("instruction");
        engine.tick();
        assert!(matches!(
            engine.board().agents[0].session,
            SessionState::Running { .. }
        ));
    }

    #[test]
    fn sessions_restart_and_every_third_one_fails() {
        let mut engine = running_engine(1);
        let path = engine.instruction_path("agent-1").expect("path");
        fs::write(&path, "keep working\n").expect("instruction");
        engine
            .send(Command::Start {
                name: "agent-1".to_owned(),
            })
            .expect("start");
        for _ in 0..120 {
            engine.tick();
        }
        let board = engine.board();
        assert!(
            board.agents[0].sessions_since_start >= 3,
            "{:?}",
            board.agents[0]
        );
        assert!(
            board.agents[0]
                .recent
                .iter()
                .any(|end| matches!(end, SessionEnd::Failed { .. }))
        );
        assert!(
            board.agents[0]
                .recent
                .iter()
                .any(|end| matches!(end, SessionEnd::Succeeded))
        );
    }

    #[test]
    fn a_notice_is_read_once_and_shutdown_stops_everything() {
        let mut engine = running_engine(2);
        engine.send(Command::AddAgent).expect("add");
        assert!(engine.board().notice.is_some());
        assert!(engine.board().notice.is_none());
        engine.shutdown(Duration::from_millis(10));
        assert!(engine.shutdown_called());
        let board = engine.board();
        assert_eq!(board.forgejo.state, ForgejoState::Stopped);
        assert!(
            board
                .agents
                .iter()
                .all(|agent| agent.session == SessionState::Idle)
        );
    }
}
