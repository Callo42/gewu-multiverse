//! Guards for the review corrections that have a public seam at `eba6955`.
//!
//! Each guard fails on the reviewed head: the positive refusal was `✓`, the review
//! sentence was `⊘`, and a branch-only push produced no work row. The branch-level
//! guards (the `/v1` render row, the 72-column legend, the success result row) live
//! with the screens they draw.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use gewu_multiverse::agent_run::{RunEnding, ending_for};
use gewu_multiverse::run;
use gewu_multiverse::transcript::Transcript;

fn scratch_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gewu-correction-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("the scratch root");
    root
}

#[test]
fn a_positive_refusal_is_not_a_success() {
    // The reviewed head classified this sentence `succeeded` because it listed only
    // "I could not / no permission" phrasings (review B2, wording A).
    assert_eq!(
        ending_for(
            "I left team/workspace in place rather than deleting it; my account does not have that permission."
        ),
        RunEnding::Refused
    );
}

#[test]
fn a_review_that_found_no_problem_is_a_success() {
    // The reviewed head matched `i could not` and called this success `refused`
    // (review B2, wording B).
    assert_eq!(
        ending_for("I could not find any problems; the work is complete and correct."),
        RunEnding::Succeeded
    );
}

#[test]
fn a_push_with_no_comment_or_pull_request_is_work() {
    // The reviewed head returned before reading the push witness (review N5).
    let root = scratch_root("branch-only");
    let run_dir = root.join("run-1700000000004-0001");
    fs::create_dir_all(&run_dir).expect("a run directory");
    Transcript::open(&run_dir).expect("the transcript");
    run::write_push_witness_at(
        &run_dir,
        "fix/only-push",
        "refs/remotes/origin/fix/only-push",
        Some("team/website"),
        Some("main"),
    )
    .expect("the push witness");
    assert_eq!(
        run::work_summary(&run_dir),
        vec!["branch fix/only-push in team/website".to_owned()],
        "a push without a comment or pull request left no work row"
    );
    let _ = fs::remove_dir_all(&root);
}
