//! The card's `run N` is the total from the records, not a session counter.
//!
//! The v0.1.30 home used by the upgrade walk had 3 + 2 + 2 run records; v0.1.31
//! opened with `run 0` on every card and reset to `run 1` on every start. This test
//! opens the exact record shape and checks the reconstructed total (W31-5, F5).

use std::fs;
use std::path::{Path, PathBuf};

use gewu_multiverse::engine::{self, EngineOptions};
use gewu_multiverse::engine_api::EngineApi;
use gewu_multiverse::paths::Layout;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gewu-runs-{label}-{}-{}",
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

fn write_home(root: &Path) {
    fs::write(
        root.join("multiverse.toml"),
        r#"terms_acknowledged = true
[endpoint]
url = "http://127.0.0.1:9/v1/chat/completions"
model = "demo"
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
    .expect("settings");
    for (agent, count) in [("agent-1", 3), ("agent-2", 2), ("agent-3", 2)] {
        fs::create_dir_all(root.join("agents").join(agent)).expect("agent directory");
        for index in 0..count {
            fs::create_dir_all(
                root.join("state")
                    .join(agent)
                    .join(format!("run-17000000000{index:02}-000{index}")),
            )
            .expect("run directory");
        }
    }
}

#[test]
fn the_total_run_count_comes_from_the_records_and_does_not_reset() {
    let scratch = Scratch::new("total");
    write_home(scratch.path());
    let layout = Layout::rooted_at(scratch.path().to_path_buf()).expect("layout");
    let mut options = EngineOptions::new(layout, scratch.path().join("data")).expect("options");
    options.auto_start_forgejo = false;
    let mut engine = engine::open_with(options).expect("engine");
    let view = engine.board();
    assert_eq!(view.agents.len(), 3);
    assert_eq!(view.agents[0].runs_total, 3, "agent-1's records");
    assert_eq!(view.agents[1].runs_total, 2, "agent-2's records");
    assert_eq!(view.agents[2].runs_total, 2, "agent-3's records");
    // The session counter is still a session counter; the card reads the total.
    assert_eq!(view.agents[0].sessions_since_start, 0);
}
