use std::fs;

use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::transcript::{Record, Transcript};

#[test]
fn the_production_transcript_append_refuses_a_terminal_record() {
    let root = std::env::temp_dir().join(format!(
        "gewu-append-terminal-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&root).expect("scratch");
    let mut transcript = Transcript::open(&root).expect("transcript");
    let terminal = Record::run_finished(
        "run-1",
        RunOutcome::Failed,
        None,
        0,
        Some("terminal".to_owned()),
    );
    assert!(transcript.append(&terminal).is_err());
    assert!(transcript.append_durable(&terminal).is_err());
    let _ = fs::remove_dir_all(&root);
}
