//! Live native-run evidence for wave 36 C1. Ignored by default; run with the
//! environment named in `return/REPORT.md`. The model is a task-owned loopback stub;
//! no real provider is contacted.
//!
//!   GEWU_LIVE_PRODUCT=... GEWU_LIVE_HOME=... GEWU_LIVE_DATA=... \
//!   GEWU_MODEL_URL=http://127.0.0.1:18880/v1 GEWU_PROGRAM=pi \
//!   cargo test --test fx-r36-live-native -- --ignored --nocapture

use std::time::{Duration, Instant};

use gewu_multiverse::credentials;
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::{Command, EngineApi, ForgejoState, Provisioning};
use gewu_multiverse::forgejo::ForgejoClient;
use gewu_multiverse::paths::Layout;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn wait_engine(
    engine: &mut impl EngineApi,
    what: &str,
    limit: Duration,
    mut ready: impl FnMut(&mut dyn EngineApi) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < limit {
        engine.tick();
        if ready(engine) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}: {:?}", engine.board().agents);
}

#[test]
#[ignore = "live loopback native run; see return/REPORT.md"]
fn c1_a_native_run_that_comments_and_pushes_is_ok() {
    let product = env("GEWU_LIVE_PRODUCT");
    let home = env("GEWU_LIVE_HOME");
    let data = env("GEWU_LIVE_DATA");
    let model_url = env("GEWU_MODEL_URL");
    let program = env("GEWU_PROGRAM");
    let model = std::env::var("GEWU_MODEL").unwrap_or_else(|_| "stub-model".to_owned());
    let protocol = if program == "codex" {
        "responses"
    } else {
        "chat-completions"
    };

    // The stub is task-owned and starts before the engine.
    let layout = Layout::rooted_at(std::path::Path::new(&home).join(".config/gewu-multiverse"))
        .expect("layout");
    layout.ensure_root().expect("config root");
    let mut options =
        EngineOptions::new(layout.clone(), std::path::Path::new(&data)).expect("engine options");
    options.product_binary = product.into();
    options.auto_start_forgejo = true;
    options.ready_timeout = Duration::from_secs(180);
    options.stop_grace = Duration::from_secs(2);
    let mut engine = engine::open_with(options).expect("engine opens");
    engine.send(Command::AcknowledgeTerms).expect("terms");
    engine
        .send(Command::SetConnection {
            base_url: model_url.clone(),
            protocol: "chat-completions".to_owned(),
            model: model.clone(),
            key: Some("gw-live-fixture-key".to_owned()),
        })
        .expect("connection");
    engine
        .send(Command::CreateAgents { count: 1 })
        .expect("agent");
    wait_engine(
        &mut engine,
        "Forgejo and the agent",
        Duration::from_secs(240),
        |engine| {
            let board = engine.board();
            matches!(board.forgejo.state, ForgejoState::Running)
                && board.forgejo.repositories >= 1
                && board.agents.len() == 1
                && board.agents[0].provisioning == Provisioning::Ready
        },
    );
    let board = engine.board();
    let url = board.forgejo.url.clone();
    engine
        .send(Command::SetAgentProgram {
            name: "agent-1".to_owned(),
            program: program.clone(),
            protocol: Some(protocol.to_owned()),
        })
        .expect("program");
    engine
        .send(Command::SetAgentModel {
            name: "agent-1".to_owned(),
            base_url: model_url.clone(),
            protocol: protocol.to_owned(),
            model: model.clone(),
            key: Some("gw-live-fixture-key".to_owned()),
        })
        .expect("agent model");
    let credentials_path = layout.credentials_file();
    let token = credentials::forgejo_admin_token(&credentials_path)
        .expect("admin token reads")
        .expect("admin token exists");
    let client = ForgejoClient::new(&url, token, Duration::from_secs(20)).expect("client");
    let issue = client
        .create_issue(
            "team",
            "workspace",
            "C1 live validation",
            "Comment that this issue was seen, then push a small note.",
        )
        .expect("issue");
    let paths = layout.agent("agent-1").expect("agent paths");
    std::fs::write(
        paths.instruction(),
        format!(
            "Comment on team/workspace issue #{} with one short sentence. Then add a file named agent-1.txt with one line, commit it, and push it to main. Do not merge anything. Close the issue when you are done.",
            issue.number
        ),
    )
    .expect("instruction");
    if program == "codex" {
        // This host's bubblewrap cannot set up its namespace (`bwrap: loopback:
        // Failed RTM_NEWADDR`), the exact wave 36 C2 wall. Use the documented
        // opt-out so the Codex run itself can be validated.
        engine
            .send(Command::SetAgentCodexSandbox {
                name: "agent-1".to_owned(),
                mode: "full-access".to_owned(),
            })
            .expect("codex sandbox opt-out");
    }
    engine
        .send(Command::Start {
            name: "agent-1".to_owned(),
        })
        .expect("start");
    wait_engine(
        &mut engine,
        "the first run",
        Duration::from_secs(180),
        |engine| {
            engine.board().agents[0].runs_total >= 1 && !engine.board().agents[0].recent.is_empty()
        },
    );
    let board = engine.board();
    let newest = board.agents[0].recent.last().cloned();
    engine
        .send(Command::Stop {
            name: "agent-1".to_owned(),
        })
        .expect("stop");
    let run_dir = layout
        .state_root()
        .join("agent-1")
        .read_dir()
        .expect("state dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .max()
        .expect("a run directory");
    let terminal = gewu_multiverse::transcript::terminal_record(&run_dir).expect("terminal record");
    let (outcome, reason) = match terminal {
        gewu_multiverse::transcript::Record::RunFinished {
            outcome, reason, ..
        } => (outcome, reason.unwrap_or_default()),
        other => panic!("no terminal record: {other:?}"),
    };
    eprintln!("run dir: {}", run_dir.display());
    eprintln!("board newest: {newest:?}");
    eprintln!("terminal: {outcome} — {reason}");
    let transcript = std::fs::read_to_string(run_dir.join("transcript.jsonl")).expect("transcript");
    let comments = transcript.matches("\"action\":\"commented\"").count();
    let pushes = transcript.matches("\"event\":\"tool_result\"").count();
    eprintln!("commented events: {comments}; tool results: {pushes}");
    assert_eq!(
        outcome, "succeeded",
        "the native run did not succeed: {reason}"
    );
    assert!(
        matches!(
            newest,
            Some(gewu_multiverse::engine_api::SessionEnd::Succeeded)
        ),
        "the board did not mark the run ok: {newest:?}"
    );
    assert!(
        comments >= 1 || program == "opencode",
        "the Forgejo comment was not recorded"
    );
    assert!(
        run_dir.join(gewu_multiverse::run::PUSH_FILE).is_file(),
        "the verified push witness is missing"
    );
    // The run must not have been scheduled as a fast warned retry; one run only.
    let runs = board.agents[0].runs_total;
    std::thread::sleep(Duration::from_secs(1));
    engine.tick();
    assert_eq!(
        engine.board().agents[0].runs_total,
        runs,
        "the run repeated"
    );
}
