//! The real upgrade shape: a v0.1.30 configuration root has no `last-seen.toml`.
//!
//! v0.1.30 never wrote the marker, so the first v0.1.31 open over a used home has
//! nothing to compare and — before this guard — went straight to the board. The
//! `install.sh` one-liner now records the old version before it replaces the
//! binary; this test covers the file shape the old build really leaves (settings,
//! three agents, instructions and seven run directories, no marker) and the
//! fallback for a binary replaced by other means.

use std::fs;
use std::path::{Path, PathBuf};

use gewu_multiverse::board::{Board, Screen};
use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::UpgradeView;
use gewu_multiverse::paths::Layout;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gewu-upgrade-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a scratch root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The exact files a used v0.1.30 home leaves, as observed on its release build:
/// settings (version 0.1.30 wrote no marker), three agents, three instructions and
/// seven run directories. Credentials and Forgejo data exist but are not needed to
/// choose the screen.
fn write_v0130_home(root: &Path) {
    fs::create_dir_all(root).expect("the root");
    fs::write(
        root.join("multiverse.toml"),
        r#"# gewu-multiverse settings (written by the engine)
terms_acknowledged = true

[endpoint]
url = "http://127.0.0.1:18080/v1/chat/completions"
model = "kencode-chat-1"
max_turns = 24

[forgejo]
admin_login = "mei"

[[agents]]
name = "agent-1"
desired = "stopped"

[[agents]]
name = "agent-2"
desired = "stopped"

[[agents]]
name = "agent-3"
desired = "stopped"
"#,
    )
    .expect("the old settings");
    let runs = [
        "agent-1", "agent-1", "agent-1", "agent-2", "agent-2", "agent-3", "agent-3",
    ];
    for (index, agent) in runs.iter().enumerate() {
        let agent_dir = root.join("agents").join(agent);
        fs::create_dir_all(&agent_dir).expect("the agent directory");
        fs::write(
            agent_dir.join("instruction.md"),
            "Read the issue assigned to me and do the work.\n",
        )
        .expect("the instruction");
        fs::create_dir_all(
            root.join("state")
                .join(agent)
                .join(format!("run-17000000000{index:02}-0001")),
        )
        .expect("the run directory");
    }
}

fn open(root: &Path) -> Board {
    let layout = Layout::rooted_at(root.to_path_buf()).expect("an absolute layout");
    let data_root = root.join("data");
    let mut options = EngineOptions::new(layout, data_root).expect("the engine options");
    // The upgrade screen is chosen while the engine opens; no server needs to run.
    options.auto_start_forgejo = false;
    let engine = engine::open_with(options).expect("the engine opens");
    Board::new(engine)
}

fn upgrade(board: &Board) -> Option<&UpgradeView> {
    // The board exposes the view it was built from.
    board.view().upgrade.as_ref()
}

#[test]
fn a_used_v0130_home_without_a_marker_shows_the_upgrade_screen_once() {
    let scratch = Scratch::new("used");
    let root = scratch.path();
    write_v0130_home(root);
    assert!(
        !root.join("last-seen.toml").exists(),
        "the old build wrote a marker: this is not the v0.1.30 shape"
    );
    let board = open(root);
    assert_eq!(board.screen(), Screen::Upgrade, "no upgrade screen");
    let view = upgrade(&board).expect("the upgrade view");
    assert_eq!(view.from_version, "an earlier version");
    assert_eq!(view.to_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(view.agents, 3);
    assert_eq!(view.instructions, 3);
    assert_eq!(view.runs, 7);
    assert!(view.release_url.contains("/releases"));
    // The open records this build, so the next open goes straight to the board.
    assert_eq!(
        gewu_multiverse::engine::last_seen::read(root).as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn a_fresh_home_has_no_upgrade_screen() {
    let scratch = Scratch::new("fresh");
    let board = open(scratch.path());
    assert_eq!(board.screen(), Screen::Terms);
    assert!(upgrade(&board).is_none());
}
