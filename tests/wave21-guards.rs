//! The twenty-first fix round's guards, runnable on the unfixed base tree too.
//!
//! Every test here drives only API that existed at `origin/main` (`6999c4c`,
//! `v0.1.20`), so the same file can be run against the base tree to show the defect.
//! The fixes are in `src/`; the guards read the screens and the files, not the fix's
//! own new functions, so "fails on base, passes now" is a run of this file, not a
//! claim about it.
//!
//! Findings driven here:
//!
//! - **Z-1** — the fresh-run window: the hub says `running` while the Runs screen says
//!   `unknown start / no terminal record`. The guard runs a stub with a deliberate
//!   start delay (`sleep 3` as the agent process), so the window is long enough to be
//!   read.
//! - **W41-2** — a present-but-unreadable transcript made the hub's state column read
//!   `idle` (the word for no run) while Runs said `unreadable`.
//! - **Z-3** — an instruction document or a declared-steps file that is there but
//!   cannot be read read `not there yet` / `none`.
//! - **Z-2** — at 200 columns the model column absorbed all the slack and pushed the
//!   state cell to the edge of the terminal.
//! - **Z-4** — at 200 columns the footer legend did not draw all its keys (`d remove`
//!   missing) while its own comment claimed every key fits on a wide terminal.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A configuration with one agent, enough to exercise the hub, the instruction screen
/// and the Runs list.
const CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 1

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#;

/// A fresh, empty configuration directory for one test.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-wave21-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A runner that does nothing and exits cleanly.
fn quiet_runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// A runner with a deliberate start delay: the process is alive for seconds while it
/// writes no transcript at all — the fresh-run window Z-1 names.
fn slow_runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("slow-runner.sh");
    fs::write(&path, "#!/bin/sh\nsleep 3\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

/// An app over a configuration written by the test, with a chosen agent runner.
fn app_over(root: &Path, runner: PathBuf) -> App {
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    App::with_runner(Layout::rooted_at(root.to_path_buf()), runner)
}

/// Write a model key for `scribe`, so a start is not refused for the wrong reason.
fn give_model_key(root: &Path) {
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the credentials file is written");
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

/// The line of a rendered screen that carries the agent's name.
fn row_for<'a>(screen: &'a str, needle: &str) -> &'a str {
    screen
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or("")
}

#[test]
fn the_hub_and_the_runs_row_agree_while_a_run_starts() {
    // Z-1: for the first ~1.25 s of every run the Runs screen read `unknown start / no
    // terminal record` while the hub read `running`. The runner here is a deliberate
    // start delay (`sleep 3`), so the run is alive for seconds without a transcript
    // record: the whole window is on the screen while it is read.
    let root = scratch("agree-start");
    let mut app = app_over(&root, slow_runner("agree-start"));
    // Answer the one-time acknowledgment, as a person would.
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "do one thing\n").expect("the instruction is written");
    give_model_key(&root);

    app.show(Screen::Agents);
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(app.status.contains("started"), "the run did not start: {}", app.status);
    // One poll: the process is alive, and its transcript is still empty.
    app.tick();

    app.show(Screen::Agents);
    let hub = render_at(100, 30, &app);
    let hub_row = row_for(&hub, "scribe");
    assert!(
        hub_row.contains("running") || hub_row.contains("starting"),
        "the hub does not say the run is live:\n{hub}"
    );

    app.show(Screen::Runs);
    let runs = render_at(100, 30, &app);
    assert!(
        !runs.contains("no terminal record"),
        "the Runs screen calls a live run a record that never existed:\n{runs}"
    );
    assert!(
        !runs.contains("unknown start"),
        "the Runs screen does not know when a live run started:\n{runs}"
    );
    let runs_row = row_for(&runs, "scribe");
    assert!(
        runs_row.contains("running") || runs_row.contains("starting"),
        "the Runs row and the hub disagree about the same run:\n{runs}"
    );
    app.shutdown();
}

#[test]
fn a_present_but_unreadable_record_is_not_idle_on_the_hub() {
    // W41-2: on a fresh start a corrupt transcript made the hub's state column read
    // `idle` (the word for no run) with no note, while the Runs screen said
    // `unreadable`. The record is on disk; its error is known; the two must agree.
    let root = scratch("unreadable-state");
    let mut app = app_over(&root, quiet_runner("unreadable-state"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let run_id = "run-1700000000000-0001";
    let dir = root.join("state").join("scribe").join(run_id);
    fs::create_dir_all(&dir).expect("the run directory is created");
    fs::write(
        dir.join("transcript.jsonl"),
        b"this line is not a record\n",
    )
    .expect("a corrupt transcript is written");
    app.reload();

    app.show(Screen::Agents);
    let hub = render_at(100, 30, &app);
    let hub_row = row_for(&hub, "scribe");
    assert!(
        hub_row.contains("unreadable"),
        "the hub does not say the record could not be read:\n{hub}"
    );
    assert!(
        !hub_row.contains("idle"),
        "the hub reads `idle` — the word for no run — for a record it cannot read:\n{hub}"
    );

    app.show(Screen::Runs);
    let runs = render_at(100, 30, &app);
    assert!(
        runs.contains("unreadable"),
        "the Runs screen does not say the record is unreadable:\n{runs}"
    );
    app.shutdown();
}

#[test]
fn an_instruction_that_is_there_but_unreadable_is_not_absent() {
    // Z-3: a directory where the instruction document was expected read `not there
    // yet`, the word for a file that is not there. It is there; it cannot be read.
    let root = scratch("instruction-unreadable");
    let mut app = app_over(&root, quiet_runner("instruction-unreadable"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    // A directory at the file's path: present, not readable as a document.
    fs::create_dir_all(paths.instruction()).expect("a directory stands in for the file");
    app.show(Screen::Instructions);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("could not be read"),
        "the Instruction screen does not say the document could not be read:\n{screen}"
    );
    assert!(
        !screen.contains("not there yet"),
        "a document that is there is called absent:\n{screen}"
    );
}

#[test]
fn a_steps_file_that_is_there_but_unreadable_is_not_none() {
    // Z-3: a directory where the declared-steps file was expected read `none`, the
    // word for a file with no steps. It is there; it cannot be read.
    let root = scratch("steps-unreadable");
    let mut app = app_over(&root, quiet_runner("steps-unreadable"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::create_dir_all(paths.steps_file()).expect("a directory stands in for the file");
    app.show(Screen::Instructions);
    let screen = render_at(100, 30, &app);
    assert!(
        screen.contains("could not be read"),
        "the Instruction screen does not say the steps file could not be read:\n{screen}"
    );
    assert!(
        !screen.contains("none — press t"),
        "a steps file that is there is called `none`:\n{screen}"
    );
}

#[test]
fn at_two_hundred_columns_the_hub_row_stays_a_row() {
    // Z-2: the model column absorbed every column of slack, so at 200 columns the
    // state cell sat ~180 columns from the agent name. The fields of one row must
    // stay together: the state word is drawn well before the right edge.
    let root = scratch("wide-hub");
    let mut app = app_over(&root, quiet_runner("wide-hub"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.show(Screen::Agents);
    let hub = render_at(200, 30, &app);
    let hub_row = row_for(&hub, "scribe");
    let state_at = hub_row
        .rfind("idle")
        .or_else(|| hub_row.rfind("running"))
        .unwrap_or_else(|| panic!("no state word on the hub row:\n{hub}"));
    assert!(
        state_at < 100,
        "at 200 columns the state cell is {state_at} columns from the start of the row:\n{hub}"
    );
}

#[test]
fn at_two_hundred_columns_the_footer_legend_draws_every_key() {
    // Z-4: the footer legend stopped at `d remove` at 200 columns while its own
    // comment claimed every key fits on a wide terminal.
    let root = scratch("wide-legend");
    let mut app = app_over(&root, quiet_runner("wide-legend"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.show(Screen::Agents);
    let hub = render_at(200, 30, &app);
    assert!(
        hub.contains("d remove"),
        "the legend leaves a key out at 200 columns:\n{hub}"
    );
    assert!(
        hub.contains("? all keys"),
        "the legend lost its full-list affordance:\n{hub}"
    );
}

#[test]
fn the_address_speaks_for_this_configuration_not_the_whole_machine() {
    // W41-1: the product said the Forgejo address applies to "every agent on this
    // machine" while it writes only this configuration's `[forgejo] url`. On a shared
    // machine the sentence was false; it must name the thing that is true.
    let root = scratch("address-config");
    let mut app = app_over(&root, quiet_runner("address-config"));
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.show(Screen::Forgejo);
    app.request_url_edit();
    let screen = render_at(100, 40, &app);
    assert!(
        screen.contains("in this configuration"),
        "the address prompt does not name what is true:\n{screen}"
    );
    assert!(
        !screen.contains("every agent on this machine"),
        "the address prompt still speaks for the whole machine:\n{screen}"
    );
}
