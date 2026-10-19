//! The twenty-second fix round's guards — the wave-21 findings, driven at the screen.
//!
//! The foreign-file matrix lives in `tests/foreign-file-matrix.rs` and is written
//! against base API so it can fail there. This file carries the rest of the wave-21
//! set — **C-1** (the add-agent form's submit key and refusal focus), **C-3** (every
//! non-success reason on the hub), **C-4** (what the consent record can and cannot
//! prove), **U2** (an unreadable record's reason without selection) and **U3** (an
//! older build against newer data) — through `App` and the drawn frame.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::RunOutcome;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#;

const TWO_AGENTS: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 1

[[agents]]
name = "checker"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/checker/instruction.md"
concurrency = 1
"#;

/// A fresh, empty configuration directory for one test.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("gewu-wave22-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A runner that does nothing and exits cleanly.
fn quiet_runner(root: &Path) -> PathBuf {
    let path = root.join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// An app over a configuration the test wrote, with the acknowledgment answered.
fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), quiet_runner(root));
    app.show_acknowledgment = false;
    app
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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

/// Write a run record for one agent, so the hub and the Runs list can seed from it.
fn write_run(root: &Path, agent: &str, run_id: &str, outcome: RunOutcome, reason: Option<&str>) {
    let dir = root.join("state").join(agent).join(run_id);
    fs::create_dir_all(&dir).expect("the run directory is created");
    let mut transcript =
        gewu_multiverse::transcript::Transcript::open(&dir).expect("a transcript is opened");
    transcript
        .append(&gewu_multiverse::transcript::Record::run_started(
            run_id, agent, 1, "/w",
        ))
        .expect("the opening record is appended");
    transcript
        .append(&gewu_multiverse::transcript::Record::run_finished(
            run_id,
            outcome,
            Some(1),
            1200,
            reason.map(str::to_owned),
        ))
        .expect("the terminal record is appended");
}

/// C-1: `enter` in a middle field moves on; it never submits. `ctrl-s` submits from
/// any field, and a refusal puts the cursor on the field its reason names.
#[test]
fn enter_advances_the_form_and_ctrl_s_submits_and_refusal_moves_the_cursor() {
    let root = scratch("c1");
    let mut app = app_with(&root, CONFIG);
    app.on_key(key(KeyCode::Char('n')));
    assert!(app.adding_agent());

    // Typing a name and pressing `enter` used to submit an incomplete form; the cursor
    // stayed on `agent name` and the next paste landed there. It must move on instead.
    for character in "writer".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.add_agent.as_ref().map(|form| form.cursor),
        Some(1),
        "`enter` did not move to the next field"
    );
    assert!(app.adding_agent(), "`enter` submitted an incomplete form");

    // Fill the endpoint and the model, leave the required key empty.
    for character in "https://model.example.org/v1".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Tab));
    for character in "example-model".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    // `ctrl-s` submits from any field; the refusal names the model key and the cursor
    // goes to that field.
    app.on_key(ctrl_s());
    assert!(app.adding_agent(), "a refusal closed the form");
    assert!(app.status.contains("model key"), "{}", app.status);
    assert_eq!(
        app.add_agent.as_ref().map(|form| form.cursor),
        Some(3),
        "the cursor did not go to the field the reason names"
    );

    // Supply the key and submit with `ctrl-s`; the agent is created.
    for character in "gw-fixture-key".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(ctrl_s());
    assert!(
        !app.adding_agent(),
        "the complete form did not submit: {}",
        app.status
    );
    assert!(app.status.contains("created `writer`"), "{}", app.status);
}

/// C-3: a `timed_out` run names its reason on the landing screen, not only on Runs.
#[test]
fn a_timed_out_or_stopped_run_names_its_reason_on_the_hub() {
    let root = scratch("c3-hub");
    write_run(
        &root,
        "scribe",
        "run-1700000000000-0001",
        RunOutcome::TimedOut,
        Some("the run reached its timeout of 3 s and was stopped"),
    );
    let mut app = app_with(&root, CONFIG);
    app.on_key(key(KeyCode::Char('y')));
    app.reload();
    app.show(Screen::Agents);
    let hub = render_at(120, 40, &app);
    assert!(
        hub.contains("the last run of `scribe` did not go well") && hub.contains("timeout of 3 s"),
        "the hub does not name a timed-out run's reason:\n{hub}"
    );

    // The Agent screen carries the same rule: a non-success names its reason where its
    // outcome is shown.
    app.show(Screen::AgentConfig);
    let agent = render_at(120, 40, &app);
    assert!(
        agent.contains("timed_out") && agent.contains("timeout of 3 s"),
        "the Agent screen does not name a timed-out run's reason:\n{agent}"
    );
    app.shutdown();
}

/// C-4: the record is a plain file and says so; its digest is checkable.
#[test]
fn the_consent_record_states_what_it_can_and_cannot_prove() {
    use gewu_multiverse::risk::{AckState, RECORD_LIMIT};

    let root = scratch("c4");
    let path = root.join("risk-acknowledgment.toml");
    let state = gewu_multiverse::risk::record(&path).expect("the record is written");
    assert!(state.is_confirmed());
    // The record itself carries the digest of the terms it was made under.
    let text = fs::read_to_string(&path).expect("the record is readable");
    assert!(text.contains("terms_digest = \"sha256:"), "{text}");
    assert!(
        text.contains("not signed or tamper-evident"),
        "the record does not say what it is not: {text}"
    );
    // The product can check it, and says only what the check proves.
    let check = state.terms_check().expect("a digest check");
    assert!(check.contains("matches the terms shown here"), "{check}");
    // The limit is stated where the record is read: the review overlay and the hub.
    let mut app = app_with(&root, CONFIG);
    app.review_acknowledgment();
    let review = render_at(100, 40, &app);
    assert!(
        review.contains("the product does not sign it"),
        "the review overlay does not state the limit:\n{review}"
    );
    app.show_acknowledgment = false;
    app.show(Screen::Agents);
    let hub = render_at(120, 40, &app);
    assert!(
        hub.contains("plain file") && hub.contains("signed"),
        "the landing screen does not name what the record is:\n{hub}"
    );
    // The sentence is the same constant in both places and in the page map.
    assert!(RECORD_LIMIT.contains("does not sign it"), "{RECORD_LIMIT}");
    let _ = AckState::read(&path);
    app.shutdown();
}

/// U2: an unreadable record's reason is drawn even when that agent is not selected.
#[test]
fn an_unreadable_record_names_its_reason_without_selection() {
    let root = scratch("u2");
    // scribe's record is present but cannot be read; checker is selected.
    let run = root
        .join("state")
        .join("scribe")
        .join("run-1700000000000-0001");
    fs::create_dir_all(&run).expect("the run directory is created");
    fs::write(run.join("transcript.jsonl"), b"this line is not a record\n")
        .expect("a corrupt transcript is written");
    let mut app = app_with(&root, TWO_AGENTS);
    app.on_key(key(KeyCode::Char('y')));
    app.reload();
    app.show(Screen::Agents);
    // Select the other agent.
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.selected_name().as_deref(), Some("checker"));
    let hub = render_at(120, 40, &app);
    assert!(
        hub.contains("the newest run record of `scribe` is there but could not be read"),
        "the reason is drawn only for the selected agent:\n{hub}"
    );
    app.shutdown();
}

/// U3: a build older than the configuration's own data says so, instead of silence.
#[test]
fn an_older_build_against_newer_data_says_so() {
    let root = scratch("u3");
    // The configuration was last opened by a version newer than this build; the record
    // is written before the app starts, exactly as a newer release would have left it.
    fs::write(
        root.join("last-seen.toml"),
        "version = \"0.1.99\"\nopened_at_unix = 1700000000\n",
    )
    .expect("the last-seen record is written");
    // A real acknowledgment record, so the acknowledgment line's own comparison is on
    // the screen too.
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the acknowledgment record is written");
    let mut app = app_with(&root, CONFIG);
    app.on_key(key(KeyCode::Char('y')));
    app.reload();
    app.show(Screen::Agents);
    let hub = render_at(140, 50, &app);
    assert!(
        hub.contains("used by version 0.1.99") && hub.contains("newer than this build"),
        "an older build against newer data says nothing:\n{hub}"
    );
    // The acknowledgment line carries the same comparison in its own words.
    assert!(
        hub.contains("used by version 0.1.99 since"),
        "the acknowledgment line does not carry the comparison:\n{hub}"
    );
    app.shutdown();
}
