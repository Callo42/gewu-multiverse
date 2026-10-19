//! Guards for the twenty-fifth fix round's three classes, each driven at the surface
//! a person reads:
//!
//! 1. the agent the add-agent form just wrote is the one the next `e`/`s` acts on;
//! 2. a start called off while it waited is named as a start, not as the agent's
//!    last run, and never raises a missing-record notice for a record it never wrote;
//! 3. a model key the endpoint refused names the product's own key-setting route,
//!    without printing a value and without rewriting the run's history.
//!
//! Every guard here is driven through `App` and a drawn frame, the same surfaces the
//! walkers read.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, EndpointFailure, EndpointTest, Screen};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::transcript::{Record, Transcript};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// One configured agent, with a per-agent instruction file.
const ONE_AGENT: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "one"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/one/instruction.md"
concurrency = 1
"#;

/// Two agents under a run limit of one, so the second start waits its turn.
const QUEUED: &str = r#"
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

/// One agent, `two`, so a record left by a since-removed agent `one` is history the
/// configuration no longer lists.
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

/// A fresh, empty configuration directory for one test.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("gewu-wave25-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A runner that does nothing and exits cleanly, so a start becomes a fast success.
///
/// It lives **outside** the configuration root: the product holds every plain file
/// under its root to 0600, so a script written there could never be executed.
fn quiet_runner() -> PathBuf {
    let dir = scratch("runner");
    let path = dir.join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// An app over a configuration the test wrote, with the first-run overlay dismissed.
fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), quiet_runner());
    app.show_acknowledgment = false;
    app
}

/// Confirm the risk record, so a start is not refused before it is prepared.
fn confirm_risk(root: &Path, app: &mut App) {
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
}

/// Draw one frame at a chosen size and read the cells back as text.
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

/// A drawn frame's text with its line wraps flattened, for a sentence the panel may
/// break across rows.
fn flat(screen: &str) -> String {
    screen.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

/// Open the form and type one string into each field, in order.
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

/// The form values for a second agent with a synthetic key.
const SECOND: [&str; 6] = [
    "two",
    "https://model.example.org/v1",
    "example-model",
    "gw-fixture-key-two",
    "",
    "",
];

/// Write a terminal run record for one agent, so the on-disk history is real.
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

/// A runner held open by the test, one gate file per agent.
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

/// Wait for the runner to create its `…-started` gate.
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

/// A queued fixture: two agents under a limit of one, each with a prior successful
/// run on disk, and neither started yet.
fn queued_app(root: &Path) -> (App, PathBuf) {
    queued_app_with(root, true)
}

/// The same fixture, optionally with a real prior run for each row.
///
/// `with_history = false` is the shape that proves the never-written reserved start
/// id is never mistaken for a vanished record: there is no real record to check.
fn queued_app_with(root: &Path, with_history: bool) -> (App, PathBuf) {
    fs::write(root.join("config.toml"), QUEUED).expect("the configuration is written");
    let (runner, gate) = gated_runner();
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), runner);
    app.show_acknowledgment = false;
    confirm_risk(root, &mut app);
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
    if with_history {
        // A real prior run for every row, so "the last run" is a fact that must
        // survive a later called-off start.
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
    }
    app.reload();
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("one"));
    (app, gate)
}

/// After the form writes a second agent, the selection, the highlight, the editor
/// target and the start target all name that agent, and the first agent's instruction
/// is not touched.
#[test]
fn a_created_agent_becomes_the_edit_and_start_target() {
    let root = scratch("created-target");
    let mut app = app_with(&root, ONE_AGENT);
    confirm_risk(&root, &mut app);
    let one_paths = app.ensure_agent("one").expect("one is prepared");
    fs::write(one_paths.instruction(), "one's own words\n").expect("one's instruction");

    fill_form(&mut app, SECOND);
    app.on_key(ctrl_s());
    assert!(
        app.status.contains("created `two`"),
        "the form did not report the new agent: {}",
        app.status
    );
    assert_eq!(
        app.selected_name().as_deref(),
        Some("two"),
        "the selection did not follow the agent the receipt named"
    );

    // The highlight follows the selection: the row with `>` is `two`.
    let screen = render_at(80, 24, &app);
    let marked = screen
        .lines()
        .find(|line| line.contains('>') && line.contains("two"))
        .unwrap_or_else(|| panic!("no highlighted `two` row:\n{screen}"));
    assert!(
        !screen
            .lines()
            .any(|line| line.contains('>') && line.contains("one")),
        "the highlight stayed on `one`:\n{screen}"
    );
    assert!(marked.contains("two"), "the highlighted row is not two");

    // `e` names the new agent, and the old agent's instruction is untouched.
    app.on_key(key(KeyCode::Char('e')));
    assert!(
        app.status.contains("instruction of `two`"),
        "e targeted the wrong agent: {}",
        app.status
    );
    assert_eq!(
        fs::read_to_string(one_paths.instruction()).expect("one's instruction is readable"),
        "one's own words\n",
        "the first agent's instruction was changed"
    );
    app.on_key(key(KeyCode::Esc));

    // `s` names the new agent too.
    let two_paths = app.ensure_agent("two").expect("two is prepared");
    fs::write(two_paths.instruction(), "two's own words\n").expect("two's instruction");
    app.on_key(key(KeyCode::Char('s')));
    assert!(
        app.status.contains("started `two`"),
        "s targeted the wrong agent: {}",
        app.status
    );
    assert!(
        !app.status.contains("started `one`"),
        "s started the first agent: {}",
        app.status
    );
    app.shutdown();
}

/// A refused or cancelled form leaves the previous selection and instructions alone.
#[test]
fn a_refused_or_cancelled_form_keeps_the_previous_selection() {
    let root = scratch("form-preserves");
    let root = &root;
    let mut app = app_with(root, ONE_AGENT);
    let one_paths = app.ensure_agent("one").expect("one is prepared");
    fs::write(one_paths.instruction(), "one's own words\n").expect("one's instruction");

    // A refusal: no name.
    let mut blank = SECOND;
    blank[0] = "";
    fill_form(&mut app, blank);
    app.on_key(ctrl_s());
    assert!(
        app.add_agent.is_some(),
        "the refusal closed the form instead of standing"
    );
    assert_eq!(
        app.selected_name().as_deref(),
        Some("one"),
        "a refusal moved the selection"
    );

    // A cancel: esc.
    app.on_key(key(KeyCode::Esc));
    assert_eq!(
        app.selected_name().as_deref(),
        Some("one"),
        "a cancel moved the selection"
    );
    assert_eq!(
        fs::read_to_string(one_paths.instruction()).expect("one's instruction is readable"),
        "one's own words\n",
        "the first agent's instruction was changed"
    );
    app.shutdown();
}

/// A called-off waiting start is a start, not the agent's last run, and never raises
/// a missing-record notice for a record it never wrote.
#[test]
fn a_called_off_waiting_start_is_not_the_last_run() {
    let root = scratch("called-off");
    let (mut app, gate) = queued_app(&root);

    // Start `one`, queue `two`.
    app.on_key(key(KeyCode::Char('s')));
    assert!(app.status.contains("started `one`"), "{}", app.status);
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_name().as_deref(), Some("two"));
    app.on_key(key(KeyCode::Char('s')));
    assert!(app.status.contains("waiting its turn"), "{}", app.status);
    wait_for_file(&gate.join("one-started"));

    // Change of mind: call the waiting start off.
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.status.contains("called off while it waited"),
        "the start was not called off: {}",
        app.status
    );
    assert_eq!(
        app.supervisor.state("two").word(),
        "start called off",
        "the row called a called-off start something else"
    );
    assert!(
        app.last_run_failure("two").is_none(),
        "the called-off start was drawn as a failed run"
    );

    // The called-off start wrote no record, so no later read may claim one vanished.
    app.reload();
    assert!(
        !app.record_forgotten("two"),
        "a record that was never written was reported as gone"
    );
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("the last start of `two` was called off"),
        "the call-off is not named as a start:\n{screen}"
    );
    assert!(
        !screen.contains("no longer there"),
        "a spurious missing-record notice was drawn:\n{screen}"
    );
    assert!(
        !screen.contains("the last run of `two` did not go well"),
        "the called-off start was drawn as the last run:\n{screen}"
    );

    // The Agent screen tells the same story: a start and its id, not a run.
    app.show(Screen::AgentConfig);
    let detail = render_at(100, 40, &app);
    assert!(
        detail.contains("start called off"),
        "the detail does not name the called-off start:\n{detail}"
    );
    assert!(
        detail.contains("last start") && detail.contains("called off before a slot was free"),
        "the detail does not give the call-off's reason:\n{detail}"
    );
    assert!(
        detail.contains("run-100-0002-1"),
        "the real prior run's id left the detail:\n{detail}"
    );
    app.shutdown();
}

/// The genuine case still warns: a record that **was** on disk and is deleted is
/// reported as gone, so the correction above never suppresses a real disappearance.
#[test]
fn a_genuine_vanished_record_still_warns() {
    let root = scratch("vanished");
    let mut app = app_with(&root, ONE_AGENT);
    let run_id = "run-100-0009-1";
    write_run(app.state_root(), "one", run_id, RunOutcome::Succeeded, None);
    app.reload();
    assert!(
        !app.record_forgotten("one"),
        "a record that is present was reported as gone"
    );

    let dir = app.state_root().join("one").join(run_id);
    fs::remove_dir_all(&dir).expect("the record is deleted by hand");
    app.reload();
    assert!(
        app.record_forgotten("one"),
        "a record that was deleted by hand was not reported"
    );
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("no longer there"),
        "the vanished-record notice was not drawn:\n{screen}"
    );
    app.shutdown();
}

/// A later, unrelated message is not overwritten by the called-off state, and a tick
/// does not resurrect the waiting receipt.
#[test]
fn a_later_message_survives_a_called_off_start() {
    let root = scratch("called-off-message");
    let (mut app, _gate) = queued_app(&root);
    app.on_key(key(KeyCode::Char('s')));
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('s')));
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.status.contains("called off while it waited"),
        "{}",
        app.status
    );

    app.on_key(key(KeyCode::Char('z')));
    assert!(
        app.status.contains("no key is bound"),
        "the unrelated key did not make its own sentence: {}",
        app.status
    );
    app.tick();
    assert!(
        app.status.contains("no key is bound"),
        "a tick overwrote the unrelated sentence: {}",
        app.status
    );
    app.shutdown();
}

/// A model key the endpoint refused names the Credentials screen's own keys where the
/// failure is drawn, keeps the cause and the agent, and rewrites no history.
#[test]
fn a_refused_key_names_the_credentials_route() {
    let root = scratch("bad-key");
    let root = &root;
    let mut app = app_with(root, ONE_AGENT);
    confirm_risk(root, &mut app);
    let reason = "the model call failed (bad_key): the endpoint refused the key (HTTP 401)";
    write_run(
        app.state_root(),
        "one",
        "run-100-0003-1",
        RunOutcome::Failed,
        Some(reason),
    );
    app.reload();

    let drawn = app.reason_with_history("one", reason);
    assert!(
        drawn.contains("press 5 and k on the Credentials screen"),
        "the refusal does not name the product's key route: {drawn}"
    );
    assert!(
        drawn.contains("select `one`"),
        "the refusal does not name whose key the route sets: {drawn}"
    );
    assert!(
        drawn.contains("the endpoint refused the key (HTTP 401)"),
        "the refusal lost its cause: {drawn}"
    );
    assert!(
        drawn.contains("`one`"),
        "the refusal does not name the agent: {drawn}"
    );
    assert!(
        !drawn.contains("gw-fixture"),
        "the refusal printed a value: {drawn}"
    );

    // The hub and the Agent screen both carry it. A narrow note area can wrap the
    // sentence at the panel border, so each half is checked rather than one unbroken
    // run across the wrap.
    let hub = flat(&render_at(100, 40, &app));
    assert!(
        hub.contains("press 5 and k")
            && hub.contains("Credentials screen")
            && hub.contains("select `one`"),
        "the hub does not carry the key route:\n{hub}"
    );
    app.show(Screen::AgentConfig);
    let detail = flat(&render_at(100, 40, &app));
    assert!(
        detail.contains("press 5 and k")
            && detail.contains("Credentials screen")
            && detail.contains("select `one`"),
        "the Agent screen does not carry the key route:\n{detail}"
    );

    // The endpoint-test line carries it too.
    let test = EndpointTest {
        agent: "one".to_owned(),
        endpoint: "https://model.example.org/v1".to_owned(),
        model: "example-model".to_owned(),
        result: Err(EndpointFailure {
            class: "bad_key".to_owned(),
            message: "the endpoint refused the key (HTTP 401)".to_owned(),
        }),
        tested_at_ms: 1_700_000_000_000,
        machine: None,
    };
    assert!(
        test.describe()
            .contains("press 5 and k on the Credentials screen"),
        "the endpoint test does not name the key route: {}",
        test.describe()
    );

    // The route works and starts nothing: `5`, `k`, a value, enter.
    app.show(Screen::Credentials);
    app.on_key(key(KeyCode::Char('k')));
    for character in "gw-fixture-key-new".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.status.contains("recorded the model key"),
        "the key route did not record the key: {}",
        app.status
    );
    assert!(
        app.supervisor.unfinished_run("one").is_none(),
        "saving a key started a run"
    );
    // The historical outcome is not rewritten by the key write.
    app.show(Screen::Agents);
    assert_eq!(
        app.last_run_failure("one").as_deref(),
        Some(reason),
        "the stored run reason was rewritten"
    );
    app.shutdown();
}

/// While a called-off start is held, a real record the hub once read and that is then
/// deleted by hand is still reported as gone, and the call-off keeps its own place.
#[test]
fn a_called_off_start_reports_a_genuinely_vanished_record() {
    let root = scratch("called-off-vanished");
    let (mut app, gate) = queued_app(&root);

    // Start `one`, queue `two`, then call `two`'s waiting start off.
    app.on_key(key(KeyCode::Char('s')));
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('s')));
    wait_for_file(&gate.join("one-started"));
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.status.contains("called off while it waited"),
        "the start was not called off: {}",
        app.status
    );

    // `two`'s real prior record was observed; nothing is reported gone yet.
    app.reload();
    assert!(
        !app.record_forgotten("two"),
        "a record still on disk was reported gone"
    );

    // Delete the real prior record, then re-read while the call-off is held.
    let real = app.state_root().join("two").join("run-100-0002-1");
    fs::remove_dir_all(&real).expect("the real record is deleted by hand");
    app.reload();
    assert!(
        app.record_forgotten("two"),
        "a real record the hub had observed vanished without a word"
    );
    let hub = flat(&render_at(100, 40, &app));
    assert!(
        hub.contains("no longer there"),
        "the vanished-record notice was not drawn:\n{hub}"
    );
    assert!(
        hub.contains("the last start of `two` was called off"),
        "the call-off was lost when its real record vanished:\n{hub}"
    );
    app.shutdown();
}

/// A called-off start never wrote a record, so its reserved id is never reported as a
/// vanished record — there is no real record for the agent at all.
#[test]
fn a_called_off_start_with_no_prior_record_reports_nothing_gone() {
    let root = scratch("called-off-nohistory");
    let (mut app, gate) = queued_app_with(&root, false);

    app.on_key(key(KeyCode::Char('s')));
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('s')));
    wait_for_file(&gate.join("one-started"));
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.status.contains("called off while it waited"),
        "the start was not called off: {}",
        app.status
    );

    // The closed record of `one` is real; `two` never wrote one. Neither the call-off
    // nor the reserved id may raise a missing-record notice.
    app.reload();
    assert!(
        !app.record_forgotten("two"),
        "the reserved start id was reported as a vanished record"
    );
    let hub = flat(&render_at(100, 40, &app));
    assert!(
        !hub.contains("no longer there"),
        "a spurious vanished-record notice was drawn:\n{hub}"
    );
    app.shutdown();
}

/// The refused-key pointer names the agent whose call failed, and following it with
/// the product's own keys writes that agent's key, leaves the other agent's key alone
/// and starts nothing.
#[test]
fn a_refused_key_pointer_leads_to_the_named_agents_key() {
    let root = scratch("bad-key-target");
    let root = &root;
    let mut app = app_with(root, QUEUED);
    confirm_risk(root, &mut app);
    for name in ["one", "two"] {
        let paths = app.ensure_agent(name).expect("the agent is prepared");
        fs::write(paths.instruction(), format!("do something for {name}\n"))
            .expect("the instruction is written");
    }
    fs::write(
        root.join("credentials.toml"),
        "[model_keys]\n\"one\" = \"gw-fixture-key-one\"\n\"two\" = \"gw-fixture-key-two\"\n",
    )
    .expect("the keys are written");
    let reason = "the model call failed (bad_key): the endpoint refused the key (HTTP 401)";
    write_run(
        app.state_root(),
        "one",
        "run-100-0001-1",
        RunOutcome::Failed,
        Some(reason),
    );
    write_run(
        app.state_root(),
        "two",
        "run-100-0002-1",
        RunOutcome::Succeeded,
        None,
    );
    app.reload();
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("one"));
    // Move the selection to `two`: the failure and the pointer belong to `one` while
    // the selected agent is not `one`.
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_name().as_deref(), Some("two"));

    let hub = flat(&render_at(120, 40, &app));
    assert!(
        hub.contains("the last run of `one` did not go well"),
        "the hub did not name one's failure:\n{hub}"
    );
    assert!(
        hub.contains("select `one`") && hub.contains("to replace its model key"),
        "the pointer does not name the agent and the selection step:\n{hub}"
    );

    // Follow the pointer exactly: Agents (1), select `one`, Credentials (5), `k`.
    app.show(Screen::Agents);
    app.on_key(key(KeyCode::Char('k'))); // move up, back to `one`
    assert_eq!(app.selected_name().as_deref(), Some("one"));
    app.show(Screen::Credentials);
    app.on_key(key(KeyCode::Char('k')));
    assert!(
        app.status.contains("model key for `one`"),
        "the prompt names the wrong agent: {}",
        app.status
    );
    for character in "gw-fixture-key-new-one".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.status.contains("recorded the model key for one"),
        "the receipt names the wrong agent: {}",
        app.status
    );

    let creds = fs::read_to_string(&app.credentials_path).expect("the credentials file");
    assert!(
        creds.contains("one = \"gw-fixture-key-new-one\""),
        "one's key was not replaced:\n{creds}"
    );
    assert!(
        creds.contains("two = \"gw-fixture-key-two\""),
        "two's key was changed:\n{creds}"
    );
    assert!(
        app.supervisor.unfinished_run("one").is_none(),
        "saving a key started one"
    );
    assert!(
        app.supervisor.unfinished_run("two").is_none(),
        "saving a key started two"
    );
    // The stored reason is history and is not rewritten by the key write.
    app.show(Screen::Agents);
    assert_eq!(
        app.last_run_failure("one").as_deref(),
        Some(reason),
        "the stored run reason was rewritten"
    );
    app.shutdown();
}

/// A refused key whose agent the configuration no longer lists promises no route that
/// cannot be followed, and says why there is nothing to replace here.
#[test]
fn a_refused_key_for_a_removed_agent_promises_no_route() {
    let root = scratch("bad-key-removed");
    let mut app = app_with(&root, TWO_ONLY);
    let reason = "the model call failed (bad_key): the endpoint refused the key (HTTP 401)";
    write_run(
        app.state_root(),
        "one",
        "run-100-0001-1",
        RunOutcome::Failed,
        Some(reason),
    );
    app.reload();

    let drawn = app.reason_with_history("one", reason);
    assert!(
        drawn.contains("(bad_key)"),
        "the removed agent's history lost its cause: {drawn}"
    );
    assert!(
        !drawn.contains("press 5 and k on the Credentials screen"),
        "a removed agent was promised a route it cannot follow: {drawn}"
    );
    assert!(
        drawn.contains("no longer lists `one`"),
        "the removed agent's history does not say why there is no route: {drawn}"
    );
    app.shutdown();
}
