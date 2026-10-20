//! The engine's clock, so the session loop can be tested without sleeping.
//!
//! Production reads the monotonic clock once per tick. The tests use
//! [`FakeClock`], whose time only moves when they move it: the exact back-off
//! sequence, the 60-second boundary and the cap are all decided by this value, never
//! by how long a test happened to take.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A monotonic clock the engine reads.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that moves only when the test says so.
#[derive(Debug)]
pub struct FakeClock {
    base: Instant,
    elapsed_ms: AtomicU64,
}

impl FakeClock {
    /// Start at now.
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed_ms: AtomicU64::new(0),
        }
    }

    /// Move the clock forward.
    pub fn advance(&self, duration: Duration) {
        let millis = duration.as_millis().min(u128::from(u64::MAX)) as u64;
        self.elapsed_ms.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.elapsed_ms.load(Ordering::SeqCst))
    }
}

/// A boxed clock, which is what the engine holds.
pub type SharedClock = Arc<dyn Clock>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fake_clock_moves_only_when_told() {
        let clock = FakeClock::new();
        let start = clock.now();
        assert_eq!(clock.now(), start);
        clock.advance(Duration::from_secs(3));
        assert_eq!(clock.now(), start + Duration::from_secs(3));
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now(), start + Duration::from_millis(3500));
    }
}
