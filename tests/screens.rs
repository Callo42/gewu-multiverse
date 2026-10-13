//! The screens, rendered without a terminal.
//!
//! `ui::draw` paints whatever backend it is given, so these tests paint an in-memory
//! backend of 100 by 30 cells and read the text back. That is the same code that
//! paints the real terminal; the difference is only where the cells go.
//!
//! Run with `--nocapture` to print every screen, which is how the screen evidence in
//! the round's report was captured:
//!
//! ```text
//! cargo test --test screens -- --nocapture
//! ```

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The size of the in-memory terminal.
const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

const CONFIG: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "instructions/scribe.md"
concurrency = 2

[[agents]]
name = "checker"
model = "other-model"
endpoint = "https://model.example.org/v1"
instruction = "instructions/checker.md"
concurrency = 1
"#;

/// A fresh, empty configuration directory for one test.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = env::temp_dir().join(format!("gewu-screens-{tag}-{}-{nanos}", process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// An app over a configuration written by the test.
fn app(root: &Path) -> App {
    fs::write(root.join("config.toml"), CONFIG).expect("the test writes the configuration");
    App::new(Layout::rooted_at(root.to_path_buf()))
}

/// Draw one frame and read the cells back as text.
fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("a test terminal");
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

/// Print a screen with a heading, so `--nocapture` reads as a transcript.
fn show(label: &str, app: &App) {
    println!("===== {label} =====");
    println!("{}", render(app));
}

#[test]
fn the_first_run_puts_the_risk_in_front_and_names_what_it_means() {
    let root = scratch("first-run");
    let app = app(&root);
    let screen = render(&app);

    assert!(app.show_acknowledgment);
    assert!(screen.contains("Before the first agent runs"), "{screen}");
    assert!(screen.contains("runs as you"), "{screen}");
    assert!(screen.contains("not a sandbox"), "{screen}");
    assert!(screen.contains("credentials file"), "{screen}");
    assert!(screen.contains("I understand, record it once"), "{screen}");
    assert!(screen.contains("refused"), "{screen}");
}

#[test]
fn the_acknowledgment_is_recorded_once_and_then_stays_out_of_the_way() {
    let root = scratch("recorded-once");
    let mut first = app(&root);
    let path = root.join("risk-acknowledgment.toml");
    assert!(!path.exists());

    first.on_key(KeyEvent::from(KeyCode::Char('y')));
    assert!(path.is_file(), "the record is written on confirmation");
    assert!(first.ack_confirmed());
    assert!(!first.show_acknowledgment);

    // A second start of the same installation does not ask again.
    let second = App::new(Layout::rooted_at(root.clone()));
    assert!(!second.show_acknowledgment);
    assert!(!render(&second).contains("Before the first agent runs"));

    // A fresh installation, meaning an empty configuration directory, asks again.
    let fresh_root = scratch("recorded-once-fresh");
    let fresh = app(&fresh_root);
    assert!(fresh.show_acknowledgment);
}

#[test]
fn nothing_starts_an_agent_before_the_acknowledgment() {
    let root = scratch("gate");
    let mut app = app(&root);
    app.on_key(KeyEvent::from(KeyCode::Char('n')));
    assert!(!app.show_acknowledgment);
    assert!(!app.ack_confirmed());

    // The start key is refused, and the acknowledgment comes back on screen.
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(app.status.contains("refused"), "{}", app.status);
    assert!(app.show_acknowledgment);

    // After confirming, the start key says plainly that nothing runs yet.
    app.on_key(KeyEvent::from(KeyCode::Char('y')));
    app.on_key(KeyEvent::from(KeyCode::Char('s')));
    assert!(app.status.contains("not implemented"), "{}", app.status);
}

#[test]
fn the_credentials_screen_shows_the_path_and_the_mode_from_disk() {
    let root = scratch("credentials");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Credentials);
    let screen = render(&app);

    let path = root.join("credentials.toml");
    assert!(path.is_file());
    assert!(screen.contains(&path.display().to_string()), "{screen}");
    assert!(screen.contains("0600"), "{screen}");
    assert!(screen.contains("never read here"), "{screen}");
}

#[test]
fn every_screen_renders_and_what_is_missing_is_named_as_missing() {
    let root = scratch("screens");
    let mut app = app(&root);
    app.on_key(KeyEvent::from(KeyCode::Char('y')));

    for screen in Screen::ALL {
        app.show(screen);
        let text = render(&app);
        show(screen.title(), &app);
        assert!(!text.is_empty());
        if screen != Screen::Agents {
            assert!(
                text.contains("Placeholder"),
                "{} must say what is not there yet:\n{text}",
                screen.title()
            );
        }
    }

    // The agent list separates the two agents and says no run has happened.
    app.show(Screen::Agents);
    let agents = render(&app);
    assert!(
        agents.contains("scribe") && agents.contains("checker"),
        "{agents}"
    );
    assert!(agents.contains("never run"), "{agents}");

    // The runs screen has one line per agent, and none of them claims a run.
    app.show(Screen::Runs);
    let runs = render(&app);
    assert!(
        runs.contains("scribe — runs recorded: 0") && runs.contains("checker — runs recorded: 0"),
        "{runs}"
    );
    assert!(runs.contains("No agent has ever run"), "{runs}");
}

#[test]
fn the_forgejo_screen_shows_the_configured_url_and_role_and_claims_no_check() {
    let root = scratch("forgejo");
    let mut app = app(&root);
    app.show_acknowledgment = false;
    app.show(Screen::Forgejo);
    let screen = render(&app);
    assert!(screen.contains("https://forgejo.example.org"), "{screen}");
    assert!(screen.contains("member"), "{screen}");
    assert!(screen.contains("no request has been sent"), "{screen}");
}

#[test]
fn a_broken_configuration_is_a_message_on_screen_rather_than_a_panic() {
    let root = scratch("broken");
    fs::write(
        root.join("config.toml"),
        "[forgejo]\nurl = \"forgejo.example.org\"\nrole = \"member\"\n",
    )
    .expect("the test writes the configuration");
    let mut app = App::new(Layout::rooted_at(root));
    app.show_acknowledgment = false;
    let screen = render(&app);
    assert!(screen.contains("forgejo.url"), "{screen}");
    assert!(screen.contains("http://"), "{screen}");
}
