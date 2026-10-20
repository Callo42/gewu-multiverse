//! The twenty-third fix round's guards — wave 22's two incompletes, the runbook's key
//! and the review's truth, driven at the screen.
//!
//! Wave 22 left two findings half-closed (`C-1`'s box did not hold its own reason, and
//! the hub's reason rule was two rules) and found three more (`w44 F1` a runbook key
//! that is impossible where the procedure says it, `w44 F2`/`F3` a review overlay that
//! dropped terms and named a state that was over, `w45 F2` a relocated configuration's
//! sentence that named no next step). Each guard here drives the surface a person
//! reads, through `App` and the drawn frame.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::risk;
use gewu_multiverse::run::RunOutcome;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const ONE_AGENT: &str = r#"
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
    let root = std::env::temp_dir().join(format!("gewu-wave23-{tag}-{}-{nanos}", std::process::id()));
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

/// An app over a configuration the test wrote, with the first-run overlay dismissed.
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

/// Write a run record for one agent, so the hub can seed from it.
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
            run_id, outcome, Some(1), 1200, reason.map(str::to_owned),
        ))
        .expect("the terminal record is appended");
}

/// Whether some drawn row holds `needle` **inside the box**, i.e. on the same row as a
/// box border. A sentence that is only in the status line is not in the box (w45 F1).
fn in_a_box(screen: &str, needle: &str) -> bool {
    screen
        .lines()
        .any(|line| line.contains(needle) && line.contains('│'))
}

/// w45 F1 (C-1's fix, finished): the add-agent form's refusal reason is drawn **in the
/// box**, on the row after the field the reason names, at the geometry where it used to
/// be the row the window dropped. The status line still carries it too.
#[test]
fn the_add_agent_form_holds_its_own_refusal_reason() {
    let root = scratch("form-reason");
    let mut app = app_with(&root, ONE_AGENT);
    app.on_key(key(KeyCode::Char('n')));
    for character in "writer".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Enter));
    for character in "https://model.example.org/v1".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    app.on_key(key(KeyCode::Tab));
    for character in "example-model".chars() {
        app.on_key(key(KeyCode::Char(character)));
    }
    // Leave the required model key empty and submit from a middle field: the refusal
    // names the model key and the cursor goes there.
    app.on_key(ctrl_s());
    assert!(app.adding_agent(), "a refusal closed the form");
    assert!(app.status.contains("model key"), "{}", app.status);
    assert_eq!(app.add_agent.as_ref().map(|form| form.cursor), Some(3));

    // 80×24 is the built geometry the walk used; the reason must be inside the box.
    for (width, height) in [(80u16, 24u16), (100, 40), (120, 45)] {
        let screen = render_at(width, height, &app);
        assert!(
            in_a_box(&screen, "model key"),
            "the refusal reason is not in the box at {width}×{height}:\n{screen}"
        );
    }
    app.shutdown();
}

/// w45 F3: one rule for reasons — every row's reason is drawn for the row, naming the
/// agent, whether or not it is selected.
#[test]
fn every_rows_reason_is_drawn_for_the_row() {
    let root = scratch("reason-rows");
    write_run(
        &root,
        "scribe",
        "run-1700000000000-0001",
        RunOutcome::TimedOut,
        Some("the run reached its timeout of 3 s and was stopped"),
    );
    write_run(
        &root,
        "checker",
        "run-1700000001000-0001",
        RunOutcome::Failed,
        Some("the endpoint refused the connection"),
    );
    let mut app = app_with(&root, TWO_AGENTS);
    app.reload();
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("scribe"));
    let first = render_at(140, 50, &app);
    assert!(
        first.contains("the last run of `scribe` did not go well"),
        "the selected row's reason is missing:\n{first}"
    );
    assert!(
        first.contains("the last run of `checker` did not go well"),
        "a reason was drawn only for the selected row:\n{first}"
    );
    // Select the other agent: both reasons are still drawn, for their own rows.
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.selected_name().as_deref(), Some("checker"));
    let second = render_at(140, 50, &app);
    assert!(
        second.contains("the last run of `scribe` did not go well")
            && second.contains("the last run of `checker` did not go well"),
        "the reason rule changed with the selection:\n{second}"
    );
    app.shutdown();
}

/// w44 F1: a key that is local to a screen, pressed elsewhere, names the screen where
/// it acts — and the address prompt really is `u` on the Forgejo screen, never on the
/// Agents screen a written runbook named.
#[test]
fn a_key_bound_on_another_screen_names_that_screen() {
    let root = scratch("key-elsewhere");
    let mut app = app_with(&root, ONE_AGENT);
    app.show(Screen::Agents);
    app.on_key(key(KeyCode::Char('u')));
    assert!(
        app.status.contains("no key is bound to that"),
        "an unbound key was silent: {}",
        app.status
    );
    assert!(
        app.status.contains("Forgejo screen (4)"),
        "the refusal names no route to the key: {}",
        app.status
    );
    assert!(app.url_prompt.is_none(), "`u` opened a prompt on Agents");
    // The key the sentence names does act where it says it does.
    app.show(Screen::Forgejo);
    app.on_key(key(KeyCode::Char('u')));
    assert!(
        app.url_prompt.is_some(),
        "`u` did not open the address prompt on the Forgejo screen"
    );
    app.shutdown();
}

/// w44 F2 and F3: the read-only review draws every terms bullet it exists to re-show
/// once the window can hold it, declares an overflow only when it really drops
/// content, and titles itself as the state it is in.
#[test]
fn the_review_draws_its_terms_and_is_titled_for_its_state() {
    let root = scratch("review-truth");
    let mut app = app_with(&root, ONE_AGENT);
    risk::record(&root.join("risk-acknowledgment.toml")).expect("the record is written");
    app.reload();
    app.review_acknowledgment();
    assert!(app.ack_review, "the review is not up");

    // A window that fits the content drops nothing: every bullet of the terms is
    // drawn, so the review's whole job can be done (the geometry the walk used).
    let wide = render_at(120, 45, &app);
    for bullet in risk::LINES {
        assert!(
            wide.contains(bullet),
            "the review dropped a terms bullet: {bullet}\n{wide}"
        );
    }
    assert!(
        !wide.contains("more below"),
        "the review declared an overflow it does not have:\n{wide}"
    );
    // w44 F3: the review is not the first run; its title says what it is.
    assert!(
        wide.contains(risk::REVIEW_TITLE),
        "the review kept the first-run title:\n{wide}"
    );
    assert!(
        !wide.contains(risk::TITLE),
        "the review is titled as a state that is over:\n{wide}"
    );

    // At the advertised floor it may not fit; then it declares, and the declaration
    // rides on the review's own title.
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains(risk::REVIEW_TITLE) && floor.contains("more below"),
        "the review at the floor does not declare its own overflow:\n{floor}"
    );
    app.shutdown();
}

/// The overlay sweep: a confirmation's own answer line is never the row a floor
/// window drops in silence — it is pinned and, if anything was dropped, the title says
/// so.
#[test]
fn a_confirmation_keeps_its_answer_at_the_floor() {
    let root = scratch("confirm-floor");
    let mut app = app_with(&root, ONE_AGENT);
    app.on_key(key(KeyCode::Char('d')));
    assert!(app.confirm_remove.is_some(), "`d` did not open the confirmation");
    let floor = render_at(72, 18, &app);
    assert!(
        in_a_box(&floor, "y or enter remove it"),
        "the removal confirmation dropped its answer line at the floor:\n{floor}"
    );
    assert!(
        floor.contains("more below"),
        "the removal confirmation dropped a body line without declaring it:\n{floor}"
    );
    app.shutdown();
}

/// w45 F2: the sentence about an acknowledgment made on another machine says what is
/// known, what is not, and what to do about it — the action, not just the fact.
#[test]
fn a_record_made_on_another_machine_says_what_to_do() {
    if gewu_multiverse::machine::mark().is_none() {
        // Without a machine mark the relation cannot be `Elsewhere`; the walk's host
        // reads one.
        return;
    }
    let root = scratch("relocated-sentence");
    fs::write(
        root.join("risk-acknowledgment.toml"),
        "acknowledged = true\nacknowledged_at_unix = 1700000000\napp_version = \"0.1.22\"\nmachine = \"gewu-other-host/0000000000000000000000000000abcd\"\n",
    )
    .expect("a record from another machine is written");
    let mut app = app_with(&root, ONE_AGENT);
    app.reload();
    let line = app.ack_record_line().expect("a record has a line");
    assert!(
        line.contains("made on another machine"),
        "the move is not named: {line}"
    );
    assert!(
        line.contains("still allowed") && line.contains("remove") && line.contains("start again"),
        "the sentence names no next step: {line}"
    );
    app.show(Screen::Agents);
    let hub = render_at(160, 60, &app);
    assert!(
        hub.contains("risk recorded on") && hub.contains("R shows the record"),
        "the hub does not name the record's one line:\n{hub}"
    );
    // The way back lives with the full record facts, in the overlay that reads them.
    app.review_acknowledgment();
    let review = render_at(160, 60, &app);
    let flat = review.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("start again"),
        "the review sentence does not carry the way back:\n{review}"
    );
    app.shutdown();
}
