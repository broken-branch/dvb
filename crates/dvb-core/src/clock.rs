//! The one clock. This file is the only place the system time is read;
//! Clippy disallowed_methods rejects clock reads anywhere else.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use utoipa::ToSchema;

/// Milliseconds since the Unix epoch, UTC.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn millis(self) -> i64 {
        self.0
    }

    pub fn plus_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }
}

/// Read once per decision and pass the value on.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[expect(
        clippy::disallowed_methods,
        reason = "The core clock is the sole system-time source"
    )]
    fn now(&self) -> Timestamp {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp(i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
    }
}

/// A clock for tests: it stands still until moved.
#[derive(Debug, Default)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn at(t: Timestamp) -> Self {
        Self(AtomicI64::new(t.0))
    }

    pub fn set(&self, t: Timestamp) {
        self.0.store(t.0, Ordering::SeqCst);
    }

    pub fn advance_millis(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_moves_only_when_told() {
        let clock = FakeClock::at(Timestamp(1_000));
        assert_eq!(clock.now(), Timestamp(1_000));
        clock.advance_millis(500);
        assert_eq!(clock.now(), Timestamp(1_500));
        clock.set(Timestamp(7));
        assert_eq!(clock.now(), Timestamp(7));
    }
}
