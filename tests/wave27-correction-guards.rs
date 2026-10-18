//! Guards for the role field, overlay scrolling, state-column width and isolated-suite
//! behaviour of the supervision-mode test.
//!
//! These drive the product's own `App`, renderer and key handling at the advertised
//! 72×18 floor:
//!
//! 1. the Forgejo `role` field follows the token actually supplied and never names a
//!    dead `C`;
//! 2. the acknowledgment/review, removal, leftover-delete and editor confirmations
//!    really scroll to the rows they declare, with their answer line pinned;
//!    the add-agent form advertises only the field keys it actually handles;
//! 3. the state column keeps `failed (server_error)` whole when two legal long names
//!    compete for the width.
//!
//! The isolated-suite behaviour of the supervision-mode test is exercised by the
//! complete suite itself, under an unavailable bus.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use gewu_multiverse::app::{App, EditTarget, Screen};
use gewu_multiverse::config::Role;
use gewu_multiverse::forgejo::{RoleCheck, Viewer};
use gewu_multiverse::paths::Layout;
use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::transcript::{Record, Transcript};
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
concurrency = 2
"#;

const LONG_AGENT: &str = "release-notes-agent-0123456789";

fn long_agent_config() -> String {
    format!(
        r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "{LONG_AGENT}"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/{LONG_AGENT}/instruction.md"
concurrency = 2
"#
    )
}

const TWO_LONG_NAMES: &str = r#"
[forgejo]
url = "https://forgejo.example.org"
role = "member"

[limits]
run_timeout_secs = 30
max_concurrent_agents = 10

[[agents]]
name = "release-old-notes-v1"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/release-old-notes-v1/instruction.md"
concurrency = 2

[[agents]]
name = "release-new-notes-v1"
model = "example-model"
endpoint = "https://model.example.org/v1"
instruction = "agents/release-new-notes-v1/instruction.md"
concurrency = 2
"#;

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("gewu-r27c-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// A configuration directory whose own path is long enough to wrap the confirmation
/// prose at 72 columns, so the scrolling window is actually exercised.
fn deep_scratch(tag: &str) -> PathBuf {
    let root = scratch(tag).join("configuration-directory-with-a-deliberately-long-name");
    fs::create_dir_all(&root).expect("the deep configuration directory is created");
    root
}

fn quiet_runner(tag: &str) -> PathBuf {
    let path = scratch(tag).join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
}

fn app_with(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    let mut app = App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("runner"),
    );
    app.show_acknowledgment = false;
    gewu_multiverse::risk::record(&root.join("risk-acknowledgment.toml"))
        .expect("the risk record is written");
    app.reload();
    app
}

fn first_run_app(root: &Path, config: &str) -> App {
    fs::write(root.join("config.toml"), config).expect("the configuration is written");
    App::with_runner(
        Layout::rooted_at(root.to_path_buf()),
        quiet_runner("runner"),
    )
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

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

fn role_field(screen: &str) -> String {
    screen
        .lines()
        .find(|line| {
            let text = line.trim_start_matches(['│', ' ']);
            text.starts_with("role") && !text.starts_with("role token")
        })
        .unwrap_or_default()
        .to_owned()
}

fn write_run(root: &Path, agent: &str, run_id: &str, reason: Option<&str>) {
    let layout = Layout::rooted_at(root.to_path_buf());
    let run_dir = layout.state_root().join(agent).join(run_id);
    fs::create_dir_all(&run_dir).expect("the run directory is created");
    let mut transcript = Transcript::open(&run_dir).expect("a transcript is opened");
    transcript
        .append(&Record::run_started(run_id, agent, 4321, "/w"))
        .expect("a run starts");
    transcript
        .append(&Record::model_reply(
            run_id,
            1,
            200,
            Some("a reply".to_owned()),
            Vec::new(),
            Some("stop".to_owned()),
            None,
            "{}",
        ))
        .expect("a reply is recorded");
    transcript
        .append(&Record::run_finished(
            run_id,
            if reason.is_some() {
                RunOutcome::Failed
            } else {
                RunOutcome::Succeeded
            },
            Some(if reason.is_some() { 1 } else { 0 }),
            1200,
            reason.map(str::to_owned),
        ))
        .expect("the run ends");
    fs::write(run_dir.join(gewu_multiverse::run::LOG_FILE), "a log\n").expect("a log is written");
}

fn member_admin_check() -> RoleCheck {
    RoleCheck {
        url: "https://forgejo.example.org".to_owned(),
        role: Role::Member,
        viewer: Viewer {
            login: "admin-user".to_owned(),
            email: None,
            description: None,
            is_admin: true,
        },
        repositories: Vec::new(),
        private_visible: 0,
    }
}

// 1. Role truth on the role field itself ----------------------------------

#[test]
fn the_role_field_follows_the_supplied_token_and_gates_c_on_an_admin_token() {
    let root = scratch("role-field");
    let mut app = app_with(&root, ONE_AGENT);

    // A member check whose account is an administrator, with no administrator token:
    // the field says what the token is and names the route that can supply the other
    // view, never the dead `C`.
    app.role_check = Some(Ok(member_admin_check()));
    app.show(Screen::Forgejo);
    let screen = render_at(120, 40, &app);
    let role = role_field(&screen);
    assert!(
        role.contains("administrator account"),
        "the role field does not name the administrator standing: {role:?}\n{screen}"
    );
    assert!(
        !role.contains("C shows that view"),
        "the role field still offers a dead C: {role:?}\n{screen}"
    );
    assert!(
        role.contains("press A") || role.contains("Credentials"),
        "the role field does not name the route to the administrator token: {role:?}\n{screen}"
    );

    // A successful administrative check is titled for the token that produced it,
    // even while the configuration still says `role = "member"`.
    app.role_check = Some(Ok(RoleCheck {
        role: Role::Admin,
        ..member_admin_check()
    }));
    let screen = render_at(120, 40, &app);
    let role = role_field(&screen);
    assert!(
        role.contains("administrator"),
        "a successful administrative check still reads as a member: {role:?}\n{screen}"
    );
    assert!(
        !role.contains("member"),
        "the role field contradicts the administrative check: {role:?}\n{screen}"
    );

    // When an administrator token *is* configured, the gated `C` is true again.
    gewu_multiverse::mode::write_private(
        root.join("credentials.toml"),
        b"forgejo_admin_token = \"gw-fixture-admin\"\n",
    )
    .expect("the administrator token is written");
    app.role_check = Some(Ok(member_admin_check()));
    let screen = render_at(120, 40, &app);
    let role = role_field(&screen);
    assert!(
        role.contains("C shows that view"),
        "the field does not offer C when an administrator token exists: {role:?}\n{screen}"
    );
    assert!(
        !role.contains("press A"),
        "the field asks for a token it already has: {role:?}\n{screen}"
    );
}

// 2. Overlay declarations are true ----------------------------------------

#[test]
fn the_first_run_terms_scroll_to_their_last_bullet_with_the_answer_pinned() {
    let root = scratch("ack-scroll");
    let mut app = first_run_app(&root, ONE_AGENT);
    assert!(app.show_acknowledgment, "the first-run modal is up");
    let last = gewu_multiverse::risk::LINES
        .last()
        .copied()
        .expect("the terms have a last bullet");

    let before = render_at(72, 18, &app);
    assert!(
        !before.contains(last),
        "the floor already shows the whole terms; the guard cannot exercise scrolling:\n{before}"
    );

    let mut seen = false;
    for _ in 0..40 {
        app.on_key(key(KeyCode::Char(']')));
        let screen = render_at(72, 18, &app);
        assert!(
            app.show_acknowledgment,
            "`]` answered the modal instead of scrolling it:\n{screen}"
        );
        assert!(
            screen.contains("y — I understand, record it once"),
            "the answer line left the floor while scrolling:\n{screen}"
        );
        assert!(
            !app.status.contains("answer this screen first"),
            "`]` was answered as an invalid key instead of scrolling: {}",
            app.status
        );
        if screen.contains(last) {
            seen = true;
            break;
        }
    }
    assert!(seen, "the last terms bullet stayed unreachable at 72×18");
}

#[test]
fn the_read_only_review_scrolls_instead_of_closing_on_a_scroll_key() {
    let root = scratch("review-scroll");
    let mut app = app_with(&root, ONE_AGENT);
    app.review_acknowledgment();
    assert!(app.ack_review, "the review is up");
    let last = gewu_multiverse::risk::LINES
        .last()
        .copied()
        .expect("the terms have a last bullet");

    let before = render_at(72, 18, &app);
    assert!(
        !before.contains(last),
        "the floor already shows the whole review; the guard cannot exercise scrolling:\n{before}"
    );

    let mut seen = false;
    for _ in 0..40 {
        app.on_key(key(KeyCode::Char(']')));
        let screen = render_at(72, 18, &app);
        assert!(
            app.show_acknowledgment && app.ack_review,
            "a review scroll key closed the review:\n{screen}"
        );
        assert!(
            screen.contains("esc closes this; [ and ] scroll"),
            "the review's pinned close line left the floor:\n{screen}"
        );
        if screen.contains(last) {
            seen = true;
            break;
        }
    }
    assert!(seen, "the last review bullet stayed unreachable at 72×18");
}

#[test]
fn the_add_agent_form_advertises_only_the_field_keys_it_handles() {
    let root = scratch("form-declaration");
    let mut app = app_with(&root, ONE_AGENT);
    app.on_key(key(KeyCode::Char('n')));
    assert!(app.adding_agent(), "the form is open");
    let screen = render_at(72, 18, &app);
    let title = screen
        .lines()
        .find(|line| line.contains("add an agent"))
        .unwrap_or_default();
    assert!(
        !title.contains("press ]") && !title.contains("— ]"),
        "the form advertises a bracket key it types into a field: {title:?}\n{screen}"
    );
    assert!(
        title.contains("tab"),
        "the form does not name the key that moves to the dropped fields: {title:?}\n{screen}"
    );
}

#[test]
fn the_removal_confirmation_scrolls_to_its_dropped_lines_with_the_answer_pinned() {
    let root = deep_scratch("remove-scroll");
    let mut app = app_with(&root, &long_agent_config());
    app.on_key(key(KeyCode::Char('d')));
    assert!(app.confirm_remove.is_some(), "the confirmation is open");
    let before = render_at(72, 18, &app);
    assert!(
        before.contains("more below"),
        "the confirmation does not declare its dropped lines:\n{before}"
    );
    let needle = "press D there to delete a secret";
    assert!(
        !before.contains(needle),
        "the dropped line is already drawn; the guard cannot exercise scrolling:\n{before}"
    );

    let mut seen = false;
    for _ in 0..40 {
        app.on_key(key(KeyCode::Char(']')));
        let screen = render_at(72, 18, &app);
        assert!(
            screen.contains("y or enter remove it"),
            "the destructive answer left the floor while scrolling:\n{screen}"
        );
        if screen.contains(needle) {
            seen = true;
            break;
        }
    }
    assert!(
        seen,
        "the removal confirmation's dropped line stayed unreachable"
    );
}

#[test]
fn the_leftover_secret_confirmation_scrolls_to_its_dropped_lines() {
    let root = scratch("forget-scroll");
    let mut app = app_with(&root, ONE_AGENT);
    let mut credentials = String::from("[model_keys]\n");
    for index in 0..24 {
        credentials.push_str(&format!(
            "\"ghost-agent-{index:02}\" = \"gw-fixture-key\"\n"
        ));
    }
    gewu_multiverse::mode::write_private(app.credentials_path.clone(), credentials.as_bytes())
        .expect("the credentials file is written");
    app.show(Screen::Credentials);
    app.on_key(key(KeyCode::Char('D')));
    assert!(app.confirm_forget.is_some(), "the confirmation is open");
    let before = render_at(72, 18, &app);
    assert!(
        before.contains("more below"),
        "the leftover confirmation does not declare its dropped lines:\n{before}"
    );
    let needle = "Their agents are not configured any more";
    assert!(
        !before.contains(needle),
        "the dropped line is already drawn; the guard cannot exercise scrolling:\n{before}"
    );

    let mut seen = false;
    for _ in 0..40 {
        app.on_key(key(KeyCode::Char(']')));
        let screen = render_at(72, 18, &app);
        assert!(
            screen.contains("y or enter delete them"),
            "the delete answer left the floor while scrolling:\n{screen}"
        );
        if screen.contains(needle) {
            seen = true;
            break;
        }
    }
    assert!(
        seen,
        "the leftover confirmation's dropped line stayed unreachable"
    );
}

#[test]
fn the_editor_confirmation_scrolls_to_its_dropped_recovery_line() {
    let root = deep_scratch("editor-scroll");
    let mut app = app_with(&root, &long_agent_config());
    let paths = app.ensure_agent(LONG_AGENT).expect("the agent is prepared");
    fs::write(paths.instruction(), "do the thing\n").expect("the instruction is written");
    fs::write(
        paths.instruction().with_file_name("instruction.md.save"),
        "recover me\n",
    )
    .expect("the emergency save is written");
    app.reload();
    app.show(Screen::AgentConfig);
    app.request_edit(EditTarget::Instruction);
    assert!(app.editor_confirm.is_some(), "the confirmation is open");
    // The real confirmation is three short sentences at this width. Lengthen the
    // label it was built with, so the shared modal window really drops a row that the
    // guard can ask `]` to reveal; the renderer and key handler are the production
    // ones.
    if let Some(action) = app.editor_confirm.as_mut() {
        action.label = format!("{} END-OF-EDITOR-LABEL", "a".repeat(700));
    }
    let before = render_at(72, 18, &app);
    assert!(
        before.contains("more below"),
        "the editor confirmation does not declare its dropped lines:\n{before}"
    );
    let needle = "END-OF-EDITOR-LABEL";
    assert!(
        !before.contains(needle),
        "the dropped label tail is already drawn; the guard cannot exercise scrolling:\n{before}"
    );

    let mut seen = false;
    for _ in 0..40 {
        app.on_key(key(KeyCode::Char(']')));
        let screen = render_at(72, 18, &app);
        assert!(
            screen.contains("enter open it"),
            "the open answer left the floor while scrolling:\n{screen}"
        );
        if screen.contains(needle) {
            seen = true;
            break;
        }
    }
    assert!(
        seen,
        "the editor confirmation's recovery line stayed unreachable"
    );
}

// 3. The state column keeps its whole failure class ------------------------

#[test]
fn the_state_column_keeps_the_server_error_class_whole_beside_two_long_names() {
    let root = scratch("state-wide");
    let mut app = app_with(&root, TWO_LONG_NAMES);
    write_run(
        &root,
        "release-old-notes-v1",
        "run-1700000000000-0001",
        Some("the model call failed (server_error): the endpoint answered HTTP 500"),
    );
    app.reload();
    app.show(Screen::Agents);
    let floor = render_at(72, 18, &app);
    assert!(
        floor.contains("failed (server_error)"),
        "the state class is shortened beside two legal long names:\n{floor}"
    );

    let wide = render_at(80, 24, &app);
    assert!(
        wide.contains("failed (server_error)"),
        "the state class is shortened at 80×24:\n{wide}"
    );
    assert!(
        wide.contains("release-o") && wide.contains("release-n"),
        "both long names lost their distinguishing ends at 80×24:\n{wide}"
    );
}
