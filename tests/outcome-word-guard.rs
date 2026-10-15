//! The outcome vocabulary guard, runnable on the unfixed tree too.
//!
//! W39-2: the hub printed `timed out` while the Runs screen printed `timed_out` for the
//! same record, against the code's own "one word for one outcome, on every screen".
//! This check drives the four outcomes the code's vocabulary map defines and asserts
//! the hub's `State::word()` is exactly `RunOutcome::as_str()`. It compiles on the base
//! tree (v0.1.19) and on the fix tree, so the old words can be shown failing and the
//! new words passing.
use gewu_multiverse::run::RunOutcome;
use gewu_multiverse::supervisor::State;

#[test]
fn the_outcome_word_is_the_same_on_the_hub_and_on_runs() {
    for outcome in [
        RunOutcome::Succeeded,
        RunOutcome::Failed,
        RunOutcome::Stopped,
        RunOutcome::TimedOut,
    ] {
        let state = State::Finished {
            run_id: "run-1".to_owned(),
            outcome,
            reason: None,
            duration_ms: 0,
        };
        assert_eq!(
            state.word(),
            outcome.as_str(),
            "the hub and the Runs screen disagree about {outcome:?}"
        );
    }
}
