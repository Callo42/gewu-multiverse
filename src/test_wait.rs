#![allow(dead_code)]
//! One wait for every test condition that another thread or process must make
//! true.
//!
//! The bound here is a liveness bound, not a speed promise: a loaded computer may
//! take minutes to schedule the work, but a condition that never becomes true
//! must still fail. A test that asserts a time the product itself promises uses
//! that product bound explicitly instead of this helper.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// The longest a test waits for a condition another thread or process must make
/// true. It is deliberately far above every product timing promise; those are
/// asserted separately against the bound the product names.
pub(crate) const LIVENESS_TIMEOUT: Duration = Duration::from_secs(300);

/// How often a liveness probe is asked again.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Ask `probe` until it answers, or return `None` at the liveness bound.
fn poll<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let started = Instant::now();
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        let elapsed = started.elapsed();
        if elapsed >= LIVENESS_TIMEOUT {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL.min(LIVENESS_TIMEOUT - elapsed));
    }
}

/// Wait until `probe` returns a value, then return it.
pub(crate) fn wait_for<T>(what: &str, probe: impl FnMut() -> Option<T>) -> T {
    poll(probe).unwrap_or_else(|| panic!("timed out after {LIVENESS_TIMEOUT:?} waiting for {what}"))
}

/// Wait until `condition` is true.
pub(crate) fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    wait_for(what, || condition().then_some(()));
}

/// Wait until `condition` is true, returning false only at the liveness bound.
/// The caller must clean up and fail on false; this is for a probe that owns a
/// process the timeout path must stop.
pub(crate) fn wait_until_or(mut condition: impl FnMut() -> bool) -> bool {
    poll(|| condition().then_some(())).is_some()
}

/// Receive on `receiver` until a value arrives; a closed channel fails at once.
pub(crate) fn recv_within<T>(what: &str, receiver: &Receiver<T>) -> T {
    wait_for(what, || match receiver.try_recv() {
        Ok(value) => Some(value),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => panic!("the sender for {what} was dropped"),
    })
}
