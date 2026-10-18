//! Guards for the twenty-sixth fix round's six classes, each driven through the
//! product's own `App`, the records it reads and the frames it draws:
//!
//! 1. an unacknowledged risk names the key that reads and records the terms, and the
//!    first-run `n` is not confused with the add-agent key;
//! 2. an agent created under a removed agent's name starts with no inherited state or
//!    history, and the earlier records stay marked as an earlier agent's;
//! 3. leaving with a start waiting warns about the waiting start too;
//! 4. `enter` on a run never opens a different configured agent;
//! 5. a `server_error` failure names the product's own next step where it is drawn;
//! 6. an invalid agent name is quoted back in the refusal.
//!
//! Every check reads a drawn frame or the product's own disk records, not a helper's
//! private return value.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, EndpointFailure, EndpointTest, Screen};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const GONE_AND_KEEP: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "gone"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/gone/instruction.md"
concurrency = 1

[[agents]]
name = "keep"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/keep/instruction.md"
concurrency = 1
"#;

const TWO_ONLY: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 1

[[agents]]
name = "two"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/two/instruction.md"
concurrency = 1
"#;

const ONE_SLOT: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 1

[[agents]]
name = "one"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/one/instruction.md"
concurrency = 1

[[agents]]
name = "two"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/two/instruction.md"
concurrency = 1
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-r26-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A runner that does nothing and exits cleanly, so a screen test can start an agent
/// without starting the program itself.
fn quiet_runner() -> PathBuf {
    let dir = scratch("runner");
    let path = dir.join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// Owns the app so any exit, including a panic, still stops what the test started.
///
/// The supervisor does not stop its children when it is dropped, so a failing
/// assertion before the test's final `shutdown()` would leave the gated shell
/// alive. The guard shuts the app down on drop and then releases both gates, so
/// the child has a second way out even if its first stop never returned. It is
/// dereferenced as the app, so the test reads like the others.
struct OwnedApp {
    app: Option<App>,
    gate: PathBuf,
}

impl OwnedApp {
    fn new(app: App, gate: PathBuf) -> Self {
        Self {
            app: Some(app),
            gate,
        }
    }
}

impl std::ops::Deref for OwnedApp {
    type Target = App;

    fn deref(&self) -> &App {
        self.app
            .as_ref()
            .expect("the app is held until the guard drops")
    }
}

impl std::ops::DerefMut for OwnedApp {
    fn deref_mut(&mut self) -> &mut App {
        self.app
            .as_mut()
            .expect("the app is held until the guard drops")
    }
}

impl Drop for OwnedApp {
    fn drop(&mut self) {
        if let Some(app) = self.app.as_mut() {
            app.shutdown();
        }
        for name in ["one-go", "two-go"] {
            let _ = fs::write(self.gate.join(name), b"released\n");
        }
    }
}

/// A runner held open by the test, so a second start queues behind the first.
fn gated_runner() -> (PathBuf, PathBuf) {
    let dir = scratch("gated-runner");
    let gate = dir.join("gate");
    fs::create_dir_all(&gate).expect("the gate directory is created");
    let path = dir.join("gated-runner.sh");
    let script = format!(
        "#!/bin/sh\n\
         run_dir=\"$2\"\n\
         case \"$run_dir\" in\n\
           */one/*) touch \"{gate}/one-started\"; while [ ! -f \"{gate}/one-go\" ]; do sleep 0.02; done ;;\n\
           */two/*) touch \"{gate}/two-started\"; while [ ! -f \"{gate}/two-go\" ]; do sleep 0.02; done ;;\n\
         esac\n\
         exit 0\n",
        gate = gate.display()
    );
    fs::write(&path, script).expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    (path, gate)
}

fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), quiet_runner());
    app.show_acknowledgment = false;
    confirm_risk(root, &mut app);
    app
}

fn app_with_runner(root: &Path, config: &str, runner: PathBuf) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), runner);
    app.show_acknowledgment = false;
    confirm_risk(root, &mut app);
    app
}

fn confirm_risk(root: &Path, app: &mut App) {
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

fn render_at(width: u16, height: u16, app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|frame| gewu_multiverse::ui::draw(frame, app))
        .expect("a frame is drawn");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            text.push_str(symbol);
        }
        text.push('\n');
    }
    text
}

fn flat(screen: &str) -> String {
    screen.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn write_run(
    state_root: &Path,
    agent: &str,
    run_id: &str,
    outcome: RunOutcome,
    reason: Option<&str>,
) {
    let dir = state_root.join(agent).join(run_id);
    fs::create_dir_all(&dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&dir).expect("the transcript is opened");
    transcript
        .append(&Record::run_started(
            run_id,
            agent,
            4242,
            "/fixture/workspace",
        ))
        .expect("the opening record is written");
    transcript
        .append(&Record::run_finished(
            run_id,
            outcome,
            Some(1),
            10,
            reason.map(str::to_owned),
        ))
        .expect("the terminal record is written");
}

fn fill_form(app: &mut App, values: [&str; 6]) {
    app.on_key(key(KeyCode::Char('n')));
    for (at, value) in values.iter().enumerate() {
        if at > 0 {
            app.on_key(key(KeyCode::Tab));
        }
        for character in value.chars() {
            app.on_key(key(KeyCode::Char(character)));
        }
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        path.exists(),
        "the runner never started: {}",
        path.display()
    );
}

/// 1. The unacknowledged state names the route to the terms, the help line matches
///    that state, and the first-run `n` never doubles as the add-agent instruction.
#[test]
fn an_unacknowledged_risk_names_the_route_and_does_not_collide_with_n() {
    let root = scratch("first-run");
    let app = App::with_runner(Layout::rooted_at(root), quiet_runner());
    assert!(app.show_acknowledgment);

    // The entry frame: the modal's `n — not now` and a line telling the person to
    // press `n` for an agent must not stand together.
    let entry = flat(&render_at(100, 40, &app));
    assert!(entry.contains("n — not now"), "{entry}");
    assert!(
        !entry.contains("press n to add your first agent"),
        "the entry screen gives `n` two meanings:\n{entry}"
    );

    let mut app = app;
    app.on_key(key(KeyCode::Char('n')));
    assert!(!app.ack_confirmed());
    assert!(
        app.status.contains("press R to read the terms")
            && app.status.contains("y to record the acknowledgment"),
        "the status after `n` gives no term-recording route: {}",
        app.status
    );
    let agents = flat(&render_at(100, 40, &app));
    assert!(
        agents.contains("press R to read the terms") && agents.contains("record"),
        "the unacknowledged Agents screen gives no term-recording route:\n{agents}"
    );

    // The help line must not talk about reading acknowledged terms again when there
    // is no record to read again.
    app.on_key(key(KeyCode::Char('?')));
    let help = flat(&render_at(100, 60, &app));
    assert!(
        !help.contains("acknowledged terms again"),
        "the help claims a record that does not exist:\n{help}"
    );
    assert!(
        help.contains("R reads the terms") && help.contains("y records the acknowledgment"),
        "the help does not name how the terms are read and recorded:\n{help}"
    );
}

/// 2. Re-creating a removed agent's name leaves the new agent idle and keeps the old
///    records marked as an earlier agent's, without changing them.
#[test]
fn a_reused_name_does_not_inherit_state_or_history() {
    let root = scratch("reused-name");
    let root = &root;
    let mut app = app_with(root, GONE_AND_KEEP);
    for name in ["gone", "keep"] {
        let paths = app.ensure_agent(name).expect("the agent is prepared");
        fs::write(paths.instruction(), format!("do something for {name}\n"))
            .expect("the instruction is written");
    }
    write_run(
        app.state_root(),
        "gone",
        "run-100-0001-1",
        RunOutcome::Succeeded,
        None,
    );
    write_run(
        app.state_root(),
        "keep",
        "run-100-0002-1",
        RunOutcome::Succeeded,
        None,
    );
    app.reload();
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("gone"));

    // Remove `gone`, which leaves its records on disk.
    app.on_key(key(KeyCode::Char('d')));
    app.on_key(key(KeyCode::Char('y')));
    assert_eq!(app.agents().len(), 1, "the agent was not removed");

    // Create a new agent with the same name and a different model.
    fill_form(
        &mut app,
        [
            "gone",
            "https://model.example.org/v2",
            "example-model-v2",
            "gw-fixture-key-new",
            "",
            "",
        ],
    );
    app.on_key(ctrl_s());
    assert_eq!(
        app.agents().len(),
        2,
        "the new agent was not written: {}",
        app.status
    );

    let agents = render_at(120, 40, &app);
    let gone_row = agents
        .lines()
        .find(|line| line.contains("gone") && line.contains("example-model-v2"))
        .unwrap_or_else(|| panic!("the new `gone` row is not drawn:\n{agents}"));
    assert!(
        gone_row.contains("idle"),
        "the new agent inherited a run state before it ran: {gone_row:?}\n{agents}"
    );

    // The boundary is the product's small note in the new agent's state directory:
    // it lists the earlier run id, creates no run, and leaves the record in place.
    let note = fs::read_to_string(app.state_root().join("gone").join("previous-runs"))
        .expect("the earlier-run note is written");
    assert!(
        note.contains("run-100-0001-1"),
        "the note does not name the earlier run: {note:?}"
    );
    assert!(
        app.state_root()
            .join("gone")
            .join("run-100-0001-1")
            .is_dir(),
        "the earlier record was moved or rewritten"
    );

    // The earlier records stay visible and recognisably the earlier agent's.
    app.show(Screen::Runs);
    app.run_selected = 0;
    let runs = render_at(120, 40, &app);
    let old_row = runs
        .lines()
        .find(|line| line.contains("run-100-0001-1"))
        .unwrap_or_else(|| panic!("the old run is not listed:\n{runs}"));
    assert!(
        old_row.contains('*'),
        "the old run lost its earlier-agent marker: {old_row:?}\n{runs}"
    );
    assert!(
        flat(&runs).contains("earlier agent"),
        "the legend does not say the record belongs to an earlier agent:\n{runs}"
    );
}

/// 2b. A refused add writes no creation boundary, so a later hand re-add of the old
///     agent still owns its earlier records.
#[test]
fn a_failed_add_leaves_no_earlier_runs_boundary() {
    let root = scratch("failed-add");
    let readonly = root.with_extension("readonly.toml");
    fs::write(&readonly, TWO_ONLY).expect("the read-only configuration target");
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o444))
        .expect("the target is read-only");
    // The product can read the configuration through the symlink, and its own
    // private-tree pass skips symlinks, so the add form's write is the failing step.
    symlink(&readonly, root.join("config.toml")).expect("the configuration symlink");

    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner());
    app.show_acknowledgment = false;
    confirm_risk(&root, &mut app);
    write_run(
        app.state_root(),
        "gone",
        "run-100-0001-1",
        RunOutcome::Succeeded,
        None,
    );
    app.reload();
    let boundary = app.state_root().join("gone").join("previous-runs");
    assert!(!boundary.exists(), "the fixture starts with no boundary");

    fill_form(
        &mut app,
        [
            "gone",
            "https://model.example.org/v1",
            "example-model",
            "gw-fixture-key",
            "",
            "",
        ],
    );
    app.on_key(ctrl_s());
    assert!(
        app.adding_agent(),
        "the add form closed although the configuration was not written"
    );
    assert!(
        app.status.contains("config.toml cannot be written"),
        "the refused add gives no write failure: {}",
        app.status
    );
    assert!(
        !boundary.exists(),
        "a refused add left a `previous-runs` boundary under the name"
    );
    app.on_key(key(KeyCode::Esc));

    // Put the old agent back by hand, the path where no creation event runs. The
    // failed add must not have classified the agent's own record as an earlier
    // agent's.
    let readded = format!(
        "{TWO_ONLY}\n[[agents]]\nname = \"gone\"\nmodel = \"example-model\"\nendpoint = \"https://model.example.org/v1\"\ninstruction = \"agents/gone/instruction.md\"\nconcurrency = 1\n"
    );
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o600))
        .expect("the target is writable again");
    fs::write(&readonly, readded).expect("the old agent is hand-readded");
    app.reload();
    assert_eq!(
        app.supervisor.state("gone").word_with_reason(),
        "succeeded",
        "the old agent's own record was presented as an earlier agent's"
    );
    assert!(
        !boundary.exists(),
        "the hand re-add found a boundary written by the refused add"
    );

    // The same name can still be created through the product. That successful add
    // must write the boundary for the record that is still there, so the new agent
    // starts idle and the old record stays the earlier agent's. If no boundary were
    // written, the new agent would inherit the old record's `succeeded` here.
    app.selected = app
        .agents()
        .iter()
        .position(|agent| agent.name == "gone")
        .expect("the hand-readded agent is selectable");
    app.on_key(key(KeyCode::Char('d')));
    app.on_key(key(KeyCode::Char('y')));
    assert!(
        !app.agents().iter().any(|agent| agent.name == "gone"),
        "the hand-readded agent was not removed: {}",
        app.status
    );
    fill_form(
        &mut app,
        [
            "gone",
            "https://model.example.org/v2",
            "example-model-v2",
            "gw-fixture-key-new",
            "",
            "",
        ],
    );
    app.on_key(ctrl_s());
    assert!(
        app.agents().iter().any(|agent| agent.name == "gone"),
        "the same name could not be created again: {}",
        app.status
    );
    assert_eq!(
        app.supervisor.state("gone").word_with_reason(),
        "idle",
        "a successful add under the old name inherited the old run state"
    );
    let note = fs::read_to_string(&boundary).expect("the successful add writes the boundary");
    assert!(
        note.contains("run-100-0001-1"),
        "the successful add's boundary does not name the earlier run: {note:?}"
    );
}

/// 3. Leaving with one run in flight and one start waiting names both, and the exit
///    path still calls the waiting start off without writing a run record for it.
#[test]
fn leaving_with_a_waiting_start_names_it_in_the_warning() {
    let root = scratch("waiting-quit");
    let (runner, gate) = gated_runner();
    // The guard stops the app on any exit; a failed warning assertion must not leave
    // the gated runner waiting for a gate that the test will never touch.
    let mut app = OwnedApp::new(app_with_runner(&root, ONE_SLOT, runner), gate.clone());
    for name in ["one", "two"] {
        let paths = app.ensure_agent(name).expect("the agent is prepared");
        fs::write(paths.instruction(), format!("do something for {name}\n"))
            .expect("the instruction is written");
    }
    fs::write(
        root.join("credentials.toml"),
        "[model_keys]\n\"one\" = \"gw-fixture-key\"\n\"two\" = \"gw-fixture-key\"\n",
    )
    .expect("the keys are written");
    app.reload();
    app.show(Screen::Agents);

    app.on_key(key(KeyCode::Char('s')));
    wait_for_file(&gate.join("one-started"));
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(app.supervisor.waiting_count(), 1);

    app.on_key(key(KeyCode::Char('q')));
    assert!(
        app.status.contains("waiting start"),
        "the leave warning omits the waiting start: {}",
        app.status
    );
    assert!(
        app.status.contains("called off") || app.status.contains("calls off"),
        "the leave warning does not say the waiting start is called off: {}",
        app.status
    );
    assert!(
        app.status.contains("1 run"),
        "the leave warning lost the running run: {}",
        app.status
    );
    assert!(
        app.status.contains("`two`"),
        "the leave warning names only a count, not the waiting agent: {}",
        app.status
    );

    app.on_key(key(KeyCode::Char('q')));
    let report = app.shutdown();
    let receipt = report.describe().unwrap_or_default();
    assert!(
        receipt.contains("`two`"),
        "the receipt names only a count, not the waiting agent: {receipt}"
    );
    assert!(matches!(
        app.supervisor.state("two"),
        gewu_multiverse::supervisor::State::CalledOff { .. }
    ));
    assert!(
        gewu_multiverse::run::runs_of_agent(app.state_root(), "two").is_empty(),
        "a waiting start was written as a run"
    );
}

/// 4. `enter` on a run opens that run's own agent, and on a record that belongs to
///    an agent the configuration no longer lists it names the history keys instead of
///    falling back to the current Agents selection.
#[test]
fn enter_on_a_run_never_opens_a_different_agent() {
    let root = scratch("removed-run-enter");
    let root = &root;
    let mut app = app_with(root, TWO_ONLY);
    write_run(
        app.state_root(),
        "one",
        "run-100-0001-1",
        RunOutcome::Succeeded,
        None,
    );
    write_run(
        app.state_root(),
        "two",
        "run-100-0002-1",
        RunOutcome::Succeeded,
        None,
    );
    app.reload();
    app.show(Screen::Runs);
    app.run_selected = app
        .recent
        .iter()
        .position(|run| run.agent == "one")
        .expect("the removed agent's run is listed");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.screen,
        Screen::Runs,
        "enter on a removed agent's run left Runs for another agent"
    );
    // The whole sentence, not shared substrings: a removed name is not configured,
    // so a "not to the configured" contrast would be false here.
    assert_eq!(
        app.status,
        "run run-100-0001-1 belongs to an agent the configuration no longer lists; press T to read its transcript or o its log, or 1 to choose a configured agent",
        "enter on a removed agent's run gives the wrong sentence"
    );

    // The same contrast is true when the configuration does list the name and the
    // record predates the agent now under it.
    let root = scratch("earlier-run-enter");
    let root = &root;
    let mut app = app_with(root, GONE_AND_KEEP);
    write_run(
        app.state_root(),
        "gone",
        "run-300-0001-1",
        RunOutcome::Succeeded,
        None,
    );
    fs::write(
        app.state_root().join("gone").join("previous-runs"),
        "run-300-0001-1\n",
    )
    .expect("the creation boundary is written");
    app.reload();
    app.show(Screen::Runs);
    app.run_selected = app
        .recent
        .iter()
        .position(|run| run.agent == "gone" && run.run_id == "run-300-0001-1")
        .expect("the earlier run is listed");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.screen, Screen::Runs);
    assert_eq!(
        app.status,
        "run run-300-0001-1 belongs to an earlier agent with that name, not to the configured `gone`; press T to read its transcript or o its log, or 1 to choose a configured agent",
        "enter on an earlier agent's run gives the wrong sentence"
    );

    // A configured run opens the run's own agent even when the Agents selection is
    // elsewhere.
    let root = scratch("configured-run-enter");
    let root = &root;
    let mut app = app_with(root, GONE_AND_KEEP);
    write_run(
        app.state_root(),
        "gone",
        "run-200-0001-1",
        RunOutcome::Succeeded,
        None,
    );
    write_run(
        app.state_root(),
        "keep",
        "run-100-0002-1",
        RunOutcome::Succeeded,
        None,
    );
    app.reload();
    app.show(Screen::Agents);
    app.selected = 1;
    assert_eq!(app.selected_name().as_deref(), Some("keep"));
    app.show(Screen::Runs);
    app.run_selected = 0;
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.screen, Screen::AgentConfig);
    assert_eq!(
        app.selected_name().as_deref(),
        Some("gone"),
        "enter opened the run of one agent on another agent's screen"
    );
}

/// 5. A `server_error` failure names the product's own next step wherever the reason
///    is drawn, without starting anything.
#[test]
fn a_server_error_names_the_next_step_where_it_is_drawn() {
    let root = scratch("server-error");
    let root = &root;
    let mut app = app_with(root, TWO_ONLY);
    let reason =
        "the model call failed (server_error): the endpoint answered HTTP 500: quota exhausted";
    write_run(
        app.state_root(),
        "two",
        "run-100-0001-1",
        RunOutcome::Failed,
        Some(reason),
    );
    app.reload();

    let hub = flat(&render_at(120, 40, &app));
    assert!(
        hub.contains("press s to try again") && hub.contains("p to test the endpoint"),
        "the hub's server-error note gives no next step:\n{hub}"
    );

    app.show(Screen::AgentConfig);
    let detail = flat(&render_at(120, 40, &app));
    assert!(
        detail.contains("press s to try again") && detail.contains("p to test the endpoint"),
        "the Agent screen's server-error note gives no next step:\n{detail}"
    );

    app.show(Screen::Runs);
    let runs = flat(&render_at(120, 40, &app));
    assert!(
        runs.contains("press s to try again") && runs.contains("p to test the endpoint"),
        "the Runs screen's server-error note gives no next step:\n{runs}"
    );

    let test = EndpointTest {
        agent: "two".to_owned(),
        endpoint: "https://model.example.org/v1".to_owned(),
        model: "example-model".to_owned(),
        result: Err(EndpointFailure {
            class: "server_error".to_owned(),
            message: "the endpoint answered HTTP 500".to_owned(),
        }),
        tested_at_ms: 1_700_000_000_000,
        machine: None,
    };
    let line = test.describe();
    assert!(
        line.contains("press s to try again") && line.contains("p to test the endpoint"),
        "the endpoint-test line gives no next step: {line}"
    );
    assert!(
        app.supervisor.unfinished_run("two").is_none(),
        "drawing the next step started a run"
    );
}

/// 6. The invalid-name refusal quotes the rejected value and keeps the rule text and
///    the fields the person typed.
#[test]
fn an_invalid_name_is_quoted_back_in_the_refusal() {
    let root = scratch("invalid-name");
    let root = &root;
    let mut app = app_with(root, GONE_AND_KEEP);
    fill_form(
        &mut app,
        [
            "Scout",
            "https://model.example.org/v1",
            "example-model",
            "gw-fixture-key",
            "",
            "",
        ],
    );
    app.on_key(ctrl_s());
    assert!(
        app.adding_agent(),
        "the form closed on a refusal: {}",
        app.status
    );
    assert!(
        app.status.contains("`Scout`"),
        "the refusal does not name the rejected value: {}",
        app.status
    );
    assert!(
        app.status
            .contains("lowercase letters, digits and hyphens only"),
        "the refusal lost the rule text: {}",
        app.status
    );
    assert!(
        app.add_agent
            .as_ref()
            .is_some_and(|form| form.fields[0] == "Scout"),
        "the rejected name was not preserved in the form"
    );
}
