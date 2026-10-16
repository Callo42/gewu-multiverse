//! The twenty-fourth fix round's guards — the six classes wave 23's two walkers
//! found, each driven at the surface a person reads.
//!
//! `Tired` (ux-w46) and `Reviser` (ux-w47) converged on four classes (the form's
//! refusal row and its clearing, the status line after a run ends, the key help
//! against the bindings, and the emptied instruction) and each found one alone (the
//! `repository` field's promise, the leftover secret's caveat). Each guard here drives
//! the same surface the walker drove, through `App` and the drawn frame.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::paths::Layout;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const ONE_AGENT: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "scribe"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/scribe/instruction.md"
concurrency = 1
"#;

// The same agent, but the Forgejo address is a closed loopback port so `i` fails
// fast instead of reaching the network.
const LOOPBACK_FORGEJO: &str = r#"
[forgejo]
url = "http://127.0.0.1:9"
role = "member"

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
    let root = std::env::temp_dir().join(format!("gewu-wave24-{tag}-{}-{nanos}", std::process::id()));
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

/// The valid form values: the fields a person fills, with the two optional Forgejo
/// fields left blank.
const VALID: [&str; 6] = [
    "writer",
    "https://model.example.org/v1",
    "example-model",
    "gw-fixture-key",
    "",
    "",
];

/// Whether some drawn row holds `needle` **inside the box**.
fn in_a_box(screen: &str, needle: &str) -> bool {
    screen
        .lines()
        .any(|line| line.contains(needle) && line.contains('│'))
}

/// The drawn row of the field the cursor is on, and the row right after it.
fn row_after_field<'a>(screen: &'a str, label: &str) -> Option<(&'a str, &'a str)> {
    let rows: Vec<&str> = screen.lines().collect();
    let at = rows.iter().position(|row| row.contains(&format!("> {label}")))?;
    Some((rows[at], rows.get(at + 1).copied().unwrap_or("")))
}

/// w46 F1 / w47 F2: the add-agent form's refusal reason is drawn on the row after the
/// field it **names**, and is withdrawn by the first key that edits a field or moves
/// the cursor. Enumerated over every refusal the form can show.
#[test]
fn every_form_refusal_is_drawn_after_the_field_it_names() {
    // Every required field, one at a time: the refusal names that field, the cursor
    // goes there, and the reason is drawn on the row right after it.
    for (at, label) in [
        (0usize, "agent name"),
        (1, "model endpoint"),
        (2, "model id"),
        (3, "model key"),
    ] {
        let root = scratch(&format!("form-refusal-{at}"));
        let mut app = app_with(&root, ONE_AGENT);
        let mut values = VALID;
        values[at] = "";
        fill_form(&mut app, values);
        app.on_key(ctrl_s());
        let refusal = app
            .add_agent
            .as_ref()
            .and_then(|form| form.error.as_ref())
            .map(|refusal| (refusal.field, refusal.message.clone()))
            .unwrap_or_else(|| panic!("no refusal for an empty {label}"));
        assert_eq!(
            refusal.0,
            Some(at),
            "the refusal for an empty {label} names the wrong field: {}",
            refusal.1
        );
        assert_eq!(
            app.add_agent.as_ref().map(|form| form.cursor),
            Some(at),
            "the cursor did not go to the field the reason names"
        );

        let screen = render_at(200, 60, &app);
        let (field_row, reason_row) =
            row_after_field(&screen, label).unwrap_or_else(|| panic!("{label} is not drawn"));
        let head: String = refusal.1.chars().take(24).collect();
        assert!(
            reason_row.contains(&head),
            "the reason for an empty {label} is not on the row after it:\nfield: {field_row:?}\nnext: {reason_row:?}"
        );
        assert!(
            in_a_box(&screen, &head),
            "the reason for an empty {label} is not inside the box:\n{screen}"
        );

        // w46 F1 / w47 F2: moving the cursor withdraws the standing reason...
        app.on_key(key(KeyCode::Down));
        assert!(
            app.add_agent
                .as_ref()
                .and_then(|form| form.error.as_ref())
                .is_none(),
            "moving the cursor left the refusal standing"
        );
        // ...and so does editing a field, without the accidental submit that would
        // create the agent now that one field has a character in it.
        app.on_key(ctrl_s());
        assert!(
            app.add_agent
                .as_ref()
                .and_then(|form| form.error.as_ref())
                .is_some(),
            "the refusal was not drawn again after moving on"
        );
        app.on_key(key(KeyCode::Char('z')));
        assert!(
            app.add_agent
                .as_ref()
                .and_then(|form| form.error.as_ref())
                .is_none(),
            "editing a field left the refusal standing"
        );
        assert!(
            !app.status.contains(&refusal.1),
            "the status kept the refusal after an edit: {}",
            app.status
        );
        app.shutdown();
    }
}

/// The other two field-bearing refusals: an unusable name and a duplicate name both
/// name the name field.
#[test]
fn a_name_refusal_is_drawn_after_the_name_field() {
    let root = scratch("form-refusal-name");
    let mut app = app_with(&root, ONE_AGENT);
    // A duplicate name: the configured agent is `scribe`.
    let mut values = VALID;
    values[0] = "scribe";
    fill_form(&mut app, values);
    app.on_key(ctrl_s());
    let refusal = app
        .add_agent
        .as_ref()
        .and_then(|form| form.error.as_ref())
        .expect("a duplicate name is refused");
    assert_eq!(refusal.field, Some(0), "{}", refusal.message);
    assert!(refusal.message.contains("already exists"), "{}", refusal.message);
    let screen = render_at(200, 60, &app);
    let (_, reason_row) = row_after_field(&screen, "agent name").expect("the name field is drawn");
    let head: String = refusal.message.chars().take(24).collect();
    assert!(reason_row.contains(&head), "the reason is not after the name:\n{screen}");

    // An unusable name, from a fresh form.
    app.on_key(key(KeyCode::Esc));
    let mut values = VALID;
    values[0] = "has spaces";
    fill_form(&mut app, values);
    app.on_key(ctrl_s());
    let refusal = app
        .add_agent
        .as_ref()
        .and_then(|form| form.error.as_ref())
        .expect("an unusable name is refused");
    assert_eq!(refusal.field, Some(0), "{}", refusal.message);
    app.shutdown();
}

/// w46 F2 / w47 F1: after a run this UI started ends, the status line says what is
/// true now — the outcome — not `started … process …` in the present tense.
#[test]
fn the_status_line_after_a_run_ends_says_what_is_true_now() {
    let root = scratch("status-ended");
    let mut app = app_with(&root, ONE_AGENT);
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml")).expect("the record is written");
    app.reload();
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    fs::write(paths.instruction(), "do something\n").expect("the instruction is written");
    fs::write(
        root.join("credentials.toml"),
        "[model_keys]\n\"scribe\" = \"gw-fixture-key\"\n",
    )
    .expect("the key is written");
    app.on_key(key(KeyCode::Char('s')));
    assert!(
        app.status.contains("started `scribe`") && app.status.contains("process"),
        "the start receipt is missing: {}",
        app.status
    );

    // The quiet runner exits at once; a tick sees the run over.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !app.status.contains("ended") && std::time::Instant::now() < deadline {
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        app.status.contains("`scribe` ended: succeeded"),
        "the status still describes the start after the run ended: {}",
        app.status
    );
    assert!(
        !app.status.contains("process"),
        "the status still names a process that has exited: {}",
        app.status
    );
    app.shutdown();
}

/// w46 F3 / w47 F4 / w47 F5: the help names the movement aliases `j`/`k` (and `l`) and
/// groups `i` — which acts on every screen — with the agent keys it acts on, not under
/// the `Forgejo:` heading.
#[test]
fn the_help_names_the_movement_aliases_and_groups_the_identity_key() {
    let root = scratch("help-bindings");
    let mut app = app_with(&root, LOOPBACK_FORGEJO);
    app.on_key(key(KeyCode::Char('?')));
    let help = render_at(160, 60, &app);
    assert!(
        help.contains("up / down (or j / k)"),
        "the Moving line does not name j/k:\n{help}"
    );
    assert!(
        help.contains("right (or l)"),
        "the Screens line does not name l:\n{help}"
    );
    assert!(
        help.contains("i settle its Forgejo"),
        "the help does not name i:\n{help}"
    );
    let agent_line = help
        .lines()
        .find(|line| line.contains("An agent:"))
        .expect("the agent group is drawn");
    assert!(
        agent_line.contains("i settle"),
        "i is not grouped with the agent keys: {agent_line}"
    );
    let forgejo_line = help
        .lines()
        .find(|line| line.contains("Forgejo:"))
        .expect("the Forgejo group is drawn");
    assert!(
        !forgejo_line.contains("identity"),
        "i is still grouped under Forgejo:: {forgejo_line}"
    );
    app.on_key(key(KeyCode::Esc));

    // The aliases act where the help says they do: `j`/`k` move the selection, and `i`
    // acts on the selected agent from the Agents screen.
    let second = r#"
[forgejo]
url = "http://127.0.0.1:9"
role = "member"

[[agents]]
name = "alpha"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/alpha/instruction.md"
concurrency = 1

[[agents]]
name = "beta"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/beta/instruction.md"
concurrency = 1
"#;
    let root = scratch("help-act");
    let mut app = app_with(&root, second);
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("alpha"));
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_name().as_deref(), Some("beta"), "j did not move");
    app.on_key(key(KeyCode::Char('k')));
    assert_eq!(app.selected_name().as_deref(), Some("alpha"), "k did not move");
    app.on_key(key(KeyCode::Char('i')));
    assert!(
        !app.status.contains("no key is bound"),
        "i did not act on the Agents screen: {}",
        app.status
    );
    app.shutdown();
}

/// w46 F4: the page map's `repository` promise says what the product actually reads —
/// the repository's **configured** local git identity, not the author of the commits
/// already there.
#[test]
fn the_repository_promise_names_the_configured_identity() {
    let map = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/page-map.md"))
        .expect("the page map is readable");
    assert!(
        map.contains("configured** local git identity"),
        "the page map still promises the commits' author"
    );
    assert!(
        !map.contains("whose local git identity the commits carry"),
        "the page map still carries the old promise"
    );
}

/// w46 F5: the leftover secret's caveat says what it measures — the credentials file's
/// own last write — and that any write moves it, so an unrelated write cannot silently
/// restate it.
#[test]
fn the_leftover_caveat_names_the_file_and_its_own_write_time() {
    let root = scratch("leftover-caveat");
    let mut app = app_with(&root, ONE_AGENT);
    fs::write(
        root.join("credentials.toml"),
        "[model_keys]\n\"ghost\" = \"gw-fixture-ghost-not-a-secret\"\n\"scribe\" = \"gw-fixture-key-not-a-secret\"\n",
    )
    .expect("the credentials file is written");
    app.reload();
    app.show(Screen::Credentials);
    let screen = render_at(160, 60, &app);
    assert!(
        screen.contains("the credentials file holding it was last written"),
        "the caveat does not name the file's own write time:\n{screen}"
    );
    assert!(
        screen.contains("writing any secret moves it"),
        "the caveat does not say an unrelated write moves the time:\n{screen}"
    );
    assert!(
        !screen.contains("left by a credentials file"),
        "the caveat still reads as the secret's own age:\n{screen}"
    );
    app.shutdown();
}

/// w47 F3: a document the person emptied is drawn as empty — not as a written
/// instruction with a byte count and a sends line for bytes a run refuses.
#[test]
fn an_emptied_instruction_is_drawn_as_empty_not_as_written() {
    for (tag, body) in [("zero", ""), ("whitespace", " \n")] {
        let root = scratch(&format!("empty-instruction-{tag}"));
        let mut app = app_with(&root, ONE_AGENT);
        let paths = app.ensure_agent("scribe").expect("the agent is prepared");
        fs::write(paths.instruction(), body).expect("the document is written");
        app.show(Screen::Instructions);
        let screen = render_at(160, 60, &app);
        assert!(
            screen.contains("not there yet — press e to write it; the file is empty"),
            "a {tag} document is not drawn as empty:\n{screen}"
        );
        assert!(
            !screen.contains("a run sends"),
            "a {tag} document is drawn with a sends line:\n{screen}"
        );
        app.shutdown();
    }
}

/// The same one-agent shape, but the concurrency limit is one and there are two
/// agents: starting the second waits its turn.
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

/// A runner held open by the test, one gate file per agent.
///
/// The product passes the run directory as `--run-agent <dir>`, and the directory
/// names the agent, so the script tells `one` from `two` and waits on its own gate.
/// Its lifetime, not its speed, decides when the state moves.
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(path.exists(), "the runner never started: {}", path.display());
}

/// Tick the app until `ready` holds or the deadline passes.
fn tick_until(app: &mut App, ready: impl Fn(&App) -> bool, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !ready(app) && std::time::Instant::now() < deadline {
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    ready(app)
}

/// The queued branch's fixture: two agents under a concurrency limit of one, with
/// `one` selected and neither started yet.
fn queued_app(root: &Path) -> (App, PathBuf) {
    fs::write(root.join("config.toml"), QUEUED).expect("the configuration is written");
    let (runner, gate) = gated_runner();
    let mut app = App::with_runner(Layout::rooted_at(root.to_path_buf()), runner);
    app.show_acknowledgment = false;
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the record is written");
    app.reload();
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
    app.show(Screen::Agents);
    assert_eq!(app.selected_name().as_deref(), Some("one"));
    (app, gate)
}

/// Start `one` and queue `two`, arriving at the waiting receipt.
fn queue_two(app: &mut App) {
    app.on_key(key(KeyCode::Char('s')));
    assert!(
        app.status.contains("started `one`"),
        "the first start did not run: {}",
        app.status
    );
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_name().as_deref(), Some("two"));
    app.on_key(key(KeyCode::Char('s')));
    assert!(
        app.status.contains("waiting its turn"),
        "the second start was not queued: {}",
        app.status
    );
}

/// Release `one`, wait for `two` to take the slot, and arrive at the `started running`
/// receipt with `two` still held open.
fn two_running(app: &mut App, gate: &Path) {
    wait_for_file(&gate.join("one-started"));
    fs::write(gate.join("one-go"), b"").expect("one is released");
    assert!(
        tick_until(app, |app| app.status.contains("started running"), 20),
        "the queued run never showed as running: {}",
        app.status
    );
    wait_for_file(&gate.join("two-started"));
}

/// w46 F2 / w47 F1, the queued branch: a start that waited its turn and then ran must
/// still be rewritten when it ends. The direct start's receipt was fixed, but the
/// queued receipt was dropped the moment it became `started running`, so the ended
/// outcome never replaced it.
#[test]
fn a_queued_start_still_ends_in_the_present_tense() {
    let root = scratch("queued-status");
    let (mut app, gate) = queued_app(&root);
    queue_two(&mut app);
    two_running(&mut app, &gate);
    fs::write(gate.join("two-go"), b"").expect("two is released");

    // When `two` ends, the receipt must say so — not the live `started running`
    // sentence.
    assert!(
        tick_until(&mut app, |app| app.status.contains("`two` ended"), 20),
        "the queued run's status still names a live process after it ended: {}",
        app.status
    );
    assert!(
        app.status.contains("`two` ended: succeeded"),
        "the queued run's end is not the outcome: {}",
        app.status
    );
    assert!(
        !app.status.contains("process"),
        "the queued run's status still names a process that has exited: {}",
        app.status
    );
    app.shutdown();
}

/// The queued watch owns only the receipt it drew. A later keypress makes the status
/// say something else, and the queued run's end must never overwrite it.
#[test]
fn a_later_keypress_is_never_overwritten_by_a_queued_runs_end() {
    let root = scratch("queued-guard");
    let (mut app, gate) = queued_app(&root);
    queue_two(&mut app);
    two_running(&mut app, &gate);

    // A key the Agents screen does not bind writes the user's own sentence.
    app.on_key(key(KeyCode::Char('z')));
    assert!(
        app.status.contains("no key is bound"),
        "the keypress did not make its own sentence: {}",
        app.status
    );

    fs::write(gate.join("two-go"), b"").expect("two is released");
    assert!(
        tick_until(&mut app, |app| !app.supervisor.state("two").is_live(), 20),
        "the queued run never ended: {}",
        app.status
    );
    assert!(
        app.status.contains("no key is bound"),
        "the run's end overwrote the user's sentence: {}",
        app.status
    );
    app.shutdown();
}

/// A queued start can still be called off: the stop's own sentence owns the status
/// from there and a later tick never resurrects the waiting receipt.
#[test]
fn a_queued_start_can_still_be_called_off() {
    let root = scratch("queued-cancel");
    let (mut app, _gate) = queued_app(&root);
    queue_two(&mut app);

    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.status.contains("called off while it waited"),
        "the queued start was not called off: {}",
        app.status
    );
    app.tick();
    assert!(
        app.status.contains("called off while it waited"),
        "a tick resurrected the waiting receipt: {}",
        app.status
    );
    app.shutdown();
}
