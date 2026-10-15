//! The foreign-file matrix — fx-r22.
//!
//! The rule this file drives: **a path the product did not write is read through six
//! fates, and each fate has one true sentence.** The six are `missing`,
//! `permission-denied`, `is-a-directory`, `empty`, `non-UTF-8` and `valid`; the sites
//! are every place the product reads such a path — the instruction document, the
//! declared steps, the run transcript, the run log, the run plan, the acknowledgment
//! record, the credentials file and the configuration file.
//!
//! Two rounds claimed this class and left siblings, so this file is the enumeration:
//! every row is driven, not described. It is written against API that exists on the
//! unfixed base (`origin/main`, `v0.1.21`) as well as here, so the same file runs on
//! both trees and the failing rows are the defect.
//!
//! The rows that fail on the base are U1 (a `run.log` that is a directory shown as a
//! readable file, and `o` saying `is not there yet`) and C-2 (an unreadable run record
//! reading `unknown start` in the Runs `started (UTC)` column). The other rows are the
//! class's earlier sites (Z-3, W38-1) and the sites that collapse deliberately; they
//! are here so the enumeration is exhausted, not extended.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::KeyEvent;
use gewu_multiverse::app::{App, Screen};
use gewu_multiverse::config::Config;
use gewu_multiverse::paths::Layout;
use gewu_multiverse::risk::AckState;
use gewu_multiverse::run::{RunPlan, file_sentence};
use gewu_multiverse::transcript;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// One valid opening record, so a `valid` transcript row is a real record and not a
/// line that merely happens to be text.
const VALID_RECORD: &[u8] = b"{\"event\":\"run_started\",\"ts\":\"2023-11-14T22:13:20Z\",\"run_id\":\"run-1700000000000-0001\",\"agent\":\"scribe\",\"pid\":1,\"workspace\":\"/w\"}\n";

/// The six fates of a path the product did not write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Missing,
    Denied,
    Directory,
    Empty,
    NonUtf8,
    Valid,
}

const CASES: [Case; 6] = [
    Case::Missing,
    Case::Denied,
    Case::Directory,
    Case::Empty,
    Case::NonUtf8,
    Case::Valid,
];

impl Case {
    fn name(self) -> &'static str {
        match self {
            Case::Missing => "missing",
            Case::Denied => "permission-denied",
            Case::Directory => "is-a-directory",
            Case::Empty => "empty",
            Case::NonUtf8 => "non-UTF-8",
            Case::Valid => "valid",
        }
    }
}

/// A fresh, empty directory for one row.
fn scratch(tag: &str, case: Case) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "gewu-matrix-{tag}-{}-{}-{nanos}",
        case.name(),
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("the scratch directory is created");
    root
}

/// Put `path` into the chosen fate. The parent directory is created for every case
/// except `missing`, where the file's own absence is the point.
fn place(path: &Path, case: Case) {
    if case != Case::Missing {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the parent directory is created");
        }
    }
    match case {
        Case::Missing => {}
        Case::Denied => {
            fs::write(path, b"something\n").expect("the file is written");
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("mode 000");
        }
        Case::Directory => {
            fs::create_dir_all(path).expect("a directory stands in for the file");
        }
        Case::Empty => {
            fs::write(path, b"").expect("an empty file is written");
        }
        Case::NonUtf8 => {
            fs::write(path, [0xff, 0xfe, 0x00, 0x01]).expect("bytes that are not UTF-8");
        }
        Case::Valid => {
            fs::write(path, b"a valid line\n").expect("a readable file is written");
        }
    }
}

/// Assert that a sentence says what the row's fate is.
fn says(case: Case, site: &str, sentence: &str, needle: &str) {
    assert!(
        sentence.contains(needle),
        "the {site} site called the {case:?} case `{sentence}`, which does not name it (`{needle}`)"
    );
}

/// The six rows of the run-log sentence, driven through `run::file_sentence`.
///
/// U1's own site. The base drew a directory as a readable file with a size and let `o`
/// say `is not there yet`; the fix opens the path, so a directory and a permission
/// denial name their reason and only a readable file shows a size.
#[test]
fn the_run_log_sentence_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("log", case);
        let path = root.join("run.log");
        place(&path, case);
        let sentence = file_sentence(&path, "log", None);
        match case {
            Case::Missing => says(case, "log", &sentence, "gone, the record has no log"),
            Case::Denied => {
                says(case, "log", &sentence, "could not be read");
                says(case, "log", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "log", &sentence, "could not be read");
                says(case, "log", &sentence, "Is a directory");
                assert!(
                    !sentence.contains("KiB") && !sentence.contains(" B)"),
                    "a directory is drawn as a readable file with a size: {sentence}"
                );
            }
            Case::Empty => says(case, "log", &sentence, "(0 B)"),
            Case::NonUtf8 => says(case, "log", &sentence, "(4 B)"),
            Case::Valid => says(case, "log", &sentence, "(13 B)"),
        }
    }
}

/// The six rows of the run-transcript sentence, through `transcript::read_checked`
/// and then `run::file_sentence` — the two calls the screens make.
#[test]
fn the_transcript_sentence_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("transcript", case);
        let dir = root.join("run-1700000000000-0001");
        let path = dir.join("transcript.jsonl");
        place(&path, case);
        if case == Case::Valid {
            fs::write(&path, VALID_RECORD).expect("a real record is written");
        }
        let error = match transcript::read_checked(&dir) {
            Ok(_) => None,
            Err(source) => Some(source.to_string()),
        };
        let sentence = file_sentence(&path, "transcript", error.as_deref());
        match case {
            Case::Missing => {
                says(case, "transcript", &sentence, "gone, the record has no transcript")
            }
            Case::Denied => {
                says(case, "transcript", &sentence, "could not be read");
                says(case, "transcript", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "transcript", &sentence, "could not be read");
                says(case, "transcript", &sentence, "Is a directory");
            }
            Case::Empty => says(case, "transcript", &sentence, "(0 B)"),
            Case::NonUtf8 => says(case, "transcript", &sentence, "could not be read"),
            Case::Valid => says(case, "transcript", &sentence, " B)"),
        }
    }
}

/// The instruction document, through the screen's own summary.
#[test]
fn the_instruction_document_is_true_for_all_six_fates() {
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
    let root = scratch("instruction", Case::Valid);
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner(&root));
    app.on_key(KeyEvent::from(crossterm::event::KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    let path = paths.instruction().to_path_buf();
    for case in CASES {
        if path.exists() {
            fs::remove_file(&path).ok();
            fs::remove_dir_all(&path).ok();
        }
        place(&path, case);
        let (_, exists, bytes, error) = app.instruction_summary().expect("a summary");
        let sentence = match &error {
            Some(reason) => format!("— the record could not be read: {reason}"),
            None if !exists => "not there yet — press e to write it".to_owned(),
            None => format!("{bytes} bytes"),
        };
        match case {
            Case::Missing => says(case, "instruction", &sentence, "not there yet"),
            Case::Denied => {
                says(case, "instruction", &sentence, "could not be read");
                says(case, "instruction", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "instruction", &sentence, "could not be read");
                says(case, "instruction", &sentence, "Is a directory");
            }
            Case::Empty => says(case, "instruction", &sentence, "0 bytes"),
            // The instruction is read as bytes and sent as bytes, so bytes that are
            // not UTF-8 are not a read failure: the sentence is the true size.
            Case::NonUtf8 => says(case, "instruction", &sentence, "4 bytes"),
            Case::Valid => says(case, "instruction", &sentence, "13 bytes"),
        }
    }
    app.shutdown();
}

/// The declared steps file, through the screen's own summary.
#[test]
fn the_declared_steps_file_is_true_for_all_six_fates() {
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
    let root = scratch("steps", Case::Valid);
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner(&root));
    app.on_key(KeyEvent::from(crossterm::event::KeyCode::Char('y')));
    let paths = app.ensure_agent("scribe").expect("the agent is prepared");
    let path = paths.steps_file().to_path_buf();
    for case in CASES {
        if path.exists() {
            fs::remove_file(&path).ok();
            fs::remove_dir_all(&path).ok();
        }
        place(&path, case);
        let (_, exists, count, error) = app.steps_summary().expect("a summary");
        let sentence = match &error {
            Some(reason) => format!("— the record could not be read: {reason}"),
            None if !exists => "none — press t to declare some".to_owned(),
            None => format!("{count} shell steps, run in the workspace"),
        };
        match case {
            Case::Missing => says(case, "steps", &sentence, "none — press t"),
            Case::Denied => {
                says(case, "steps", &sentence, "could not be read");
                says(case, "steps", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "steps", &sentence, "could not be read");
                says(case, "steps", &sentence, "Is a directory");
            }
            Case::Empty => says(case, "steps", &sentence, "0 shell steps"),
            Case::NonUtf8 => says(case, "steps", &sentence, "could not be read"),
            Case::Valid => says(case, "steps", &sentence, "1 shell step"),
        }
    }
    app.shutdown();
}

/// The run plan, through the reader the start path uses.
#[test]
fn the_run_plan_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("plan", case);
        let dir = root.join("run-1700000000000-0001");
        fs::create_dir_all(&dir).expect("the run directory is created");
        let path = dir.join("plan.json");
        match case {
            // A valid plan is written by the product's own writer, so the row is the
            // real file, not a hand-made one.
            Case::Valid => RunPlan {
                run_id: "run-1700000000000-0001".to_owned(),
                agent: "scribe".to_owned(),
                workspace: root.join("workspace"),
                instruction: root.join("instruction.md"),
                skills_dir: root.join("skills"),
                endpoint: "https://model.example.org/v1".to_owned(),
                model: "example-model".to_owned(),
                credentials: root.join("credentials.toml"),
                max_turns: 4,
                steps: Vec::new(),
                timeout_secs: 60,
            }
            .write(&dir)
            .expect("the plan is written"),
            _ => place(&path, case),
        }
        let sentence = match RunPlan::read(&dir) {
            Ok(_) => "read".to_owned(),
            Err(source) => format!("the plan cannot be read: {source}"),
        };
        match case {
            Case::Missing => says(case, "plan", &sentence, "No such file"),
            Case::Denied => {
                says(case, "plan", &sentence, "cannot be read");
                says(case, "plan", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "plan", &sentence, "cannot be read");
                says(case, "plan", &sentence, "Is a directory");
            }
            Case::Empty => says(case, "plan", &sentence, "cannot be read"),
            Case::NonUtf8 => says(case, "plan", &sentence, "cannot be read"),
            Case::Valid => assert_eq!(sentence, "read"),
        }
    }
}

/// The acknowledgment record, through the reader that fails closed.
#[test]
fn the_acknowledgment_record_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("record", case);
        let path = root.join("risk-acknowledgment.toml");
        match case {
            Case::Valid => {
                fs::write(
                    &path,
                    "acknowledged = true\nacknowledged_at_unix = 1700000000\napp_version = \"0.1.21\"\n",
                )
                .expect("a real record is written");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 600");
            }
            _ => place(&path, case),
        }
        let state = AckState::read(&path);
        let sentence = state.describe();
        match case {
            // A record that is not there and one that is empty both say there is no
            // confirmation, which is true and fails closed.
            Case::Missing | Case::Empty => {
                says(case, "record", &sentence, "not acknowledged");
            }
            Case::Denied => {
                says(case, "record", &sentence, "cannot be read");
                says(case, "record", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "record", &sentence, "cannot be read");
                says(case, "record", &sentence, "Is a directory");
            }
            Case::NonUtf8 => says(case, "record", &sentence, "cannot be read"),
            Case::Valid => {
                says(case, "record", &sentence, "acknowledged on");
                says(case, "record", &sentence, "0.1.21");
            }
        }
    }
}

/// The credentials file, through the reader that hands out a secret.
#[test]
fn the_credentials_file_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("credentials", case);
        let path = root.join("credentials.toml");
        match case {
            Case::Valid => {
                fs::write(&path, "forgejo_token = \"gw-fixture-token\"\n")
                    .expect("a real credentials file is written");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 600");
            }
            // The custody rule means a readable but too-wide file is refused before its
            // key is read; the readable rows are held to 0600 so the read is reached.
            Case::Empty | Case::NonUtf8 => {
                place(&path, case);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 600");
            }
            _ => place(&path, case),
        }
        let sentence = match gewu_multiverse::credentials::forgejo_token(&path) {
            Ok(_) => "read".to_owned(),
            Err(error) => error.to_string(),
        };
        match case {
            Case::Missing => says(case, "credentials", &sentence, "is not there"),
            Case::Denied => {
                says(case, "credentials", &sentence, "cannot be read");
                says(case, "credentials", &sentence, "Permission denied");
                assert!(
                    !sentence.contains("readable by others"),
                    "a mode-000 file is told its secrets are readable by others: {sentence}"
                );
            }
            Case::Directory => {
                says(case, "credentials", &sentence, "not a regular file");
            }
            Case::Empty => says(case, "credentials", &sentence, "records no Forgejo value"),
            Case::NonUtf8 => says(case, "credentials", &sentence, "cannot be read"),
            Case::Valid => assert_eq!(sentence, "read"),
        }
    }
}

/// The configuration file, through the reader the screens use.
#[test]
fn the_configuration_file_is_true_for_all_six_fates() {
    for case in CASES {
        let root = scratch("config", case);
        let path = root.join("config.toml");
        match case {
            Case::Valid => {
                fs::write(
                    &path,
                    "[forgejo]\nurl = \"https://forgejo.example.org\"\nrole = \"member\"\n",
                )
                .expect("a real configuration is written");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode 600");
            }
            _ => place(&path, case),
        }
        let sentence = match Config::load(&path) {
            Ok(_) => "read".to_owned(),
            Err(error) => error.to_string(),
        };
        match case {
            Case::Missing => says(case, "config", &sentence, "is not there yet"),
            Case::Denied => {
                says(case, "config", &sentence, "cannot be read");
                says(case, "config", &sentence, "Permission denied");
            }
            Case::Directory => {
                says(case, "config", &sentence, "cannot be read");
                says(case, "config", &sentence, "Is a directory");
            }
            // An empty file is valid TOML with nothing in it; the configuration is then
            // refused for the field that is missing, never called absent.
            Case::Empty => {
                says(case, "config", &sentence, "not a valid configuration");
                says(case, "config", &sentence, "missing field");
                assert!(!sentence.contains("is not there yet"), "{sentence}");
            }
            Case::NonUtf8 => says(case, "config", &sentence, "cannot be read"),
            Case::Valid => assert_eq!(sentence, "read"),
        }
    }
}

/// U1 at the screen and the key: a `run.log` that is a directory must not be drawn as
/// a readable file with a size, and `o` must not call it absent.
#[test]
fn a_directory_where_the_log_should_be_is_not_shown_as_readable() {
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
    let root = scratch("log-screen", Case::Directory);
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    // A run directory the runs list can find, whose `run.log` is a directory.
    let run = root.join("state").join("scribe").join("run-1700000000000-0001");
    fs::create_dir_all(&run).expect("the run directory is created");
    fs::write(run.join("transcript.jsonl"), VALID_RECORD).expect("a real record is written");
    fs::create_dir_all(run.join("run.log")).expect("a directory stands in for the log");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner(&root));
    app.show_acknowledgment = false;
    app.reload();

    app.show(Screen::AgentConfig);
    let agent = render_at(100, 30, &app);
    assert!(
        !agent.contains("KiB") || !agent.contains("run.log"),
        "the Agent screen drew the directory as a readable file with a size:\n{agent}"
    );
    assert!(
        agent.contains("could not be read") && agent.contains("Is a directory"),
        "the Agent screen does not say the log could not be read:\n{agent}"
    );

    app.show(Screen::Runs);
    let runs = render_at(100, 50, &app);
    assert!(
        runs.contains("could not be read") && runs.contains("Is a directory"),
        "the Runs note does not say the log could not be read:\n{runs}"
    );

    app.request_view_log();
    assert!(
        app.status.contains("could not be read") && app.status.contains("Is a directory"),
        "`o` called a present directory absent: {}",
        app.status
    );
    assert!(
        !app.status.contains("is not there yet"),
        "`o` called a present directory absent: {}",
        app.status
    );
    app.shutdown();
}

/// U1's transcript sibling: `T` on a transcript that is a directory.
#[test]
fn a_directory_where_the_transcript_should_be_is_not_shown_as_readable() {
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
    let root = scratch("transcript-screen", Case::Directory);
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    let run = root.join("state").join("scribe").join("run-1700000000000-0001");
    fs::create_dir_all(&run).expect("the run directory is created");
    fs::create_dir_all(run.join("transcript.jsonl")).expect("a directory stands in for the file");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner(&root));
    app.show_acknowledgment = false;
    app.reload();
    app.show(Screen::Runs);
    let runs = render_at(100, 30, &app);
    assert!(
        runs.contains("could not be read") && runs.contains("Is a directory"),
        "the Runs note does not say the transcript could not be read:\n{runs}"
    );
    // C-2's sibling: the run id carries the start instant, so the column shows a clock
    // rather than `unknown start` even though the record cannot be read.
    assert!(
        !runs.contains("unknown start"),
        "an unreadable record read `unknown start` although its id carries the start:\n{runs}"
    );
    app.request_view_transcript();
    assert!(
        app.status.contains("could not be read") && app.status.contains("Is a directory"),
        "`T` called a present directory absent: {}",
        app.status
    );
    app.shutdown();
}

/// C-2: a present-but-unreadable run record must still show the start instant its own
/// run id carries, in both the Runs row and the Runs note.
#[test]
fn an_unreadable_record_still_shows_the_start_its_run_id_carries() {
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
    let root = scratch("start-column", Case::NonUtf8);
    fs::write(root.join("config.toml"), CONFIG).expect("the configuration is written");
    let run = root.join("state").join("scribe").join("run-1700000000000-0001");
    fs::create_dir_all(&run).expect("the run directory is created");
    fs::write(run.join("transcript.jsonl"), b"this line is not a record\n")
        .expect("a corrupt transcript is written");
    let mut app = App::with_runner(Layout::rooted_at(root.clone()), quiet_runner(&root));
    app.show_acknowledgment = false;
    app.reload();
    app.show(Screen::Runs);
    let runs = render_at(120, 30, &app);
    assert!(
        !runs.contains("unknown start"),
        "the Runs `started (UTC)` column called a record with a start-carrying id `unknown start`:\n{runs}"
    );
    // The compact UTC form of 1700000000000 ms is 2023-11-14 22:13:20Z.
    assert!(
        runs.contains("2023-11-14 22:13:20Z"),
        "the start instant the run id carries is not in the column:\n{runs}"
    );
    assert!(
        runs.contains("could not be read"),
        "the Runs note does not carry the read reason:\n{runs}"
    );
    app.shutdown();
}

/// A runner that does nothing and exits cleanly.
fn quiet_runner(root: &Path) -> PathBuf {
    let path = root.join("quiet-runner.sh");
    fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the runner is written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("runnable");
    path
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
