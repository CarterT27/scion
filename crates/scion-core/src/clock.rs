//! Injectable time source (PLAN 5.1: time comes only from `Clock`).

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A source of wall-clock time.
pub trait Clock: Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> SystemTime;
}

/// The real system clock.
#[derive(Copy, Clone, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A manually driven clock for deterministic tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<SystemTime>,
}

impl ManualClock {
    /// A clock starting at `start`.
    pub fn new(start: SystemTime) -> Arc<Self> {
        Arc::new(ManualClock {
            now: Mutex::new(start),
        })
    }

    /// Move time forward by `d`.
    pub fn advance(&self, d: Duration) {
        let mut now = self.now.lock();
        *now += d;
    }

    /// Set the current time.
    pub fn set(&self, t: SystemTime) {
        *self.now.lock() = t;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let c = ManualClock::new(start);
        assert_eq!(c.now(), start);
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now(), start + Duration::from_secs(5));
        c.set(SystemTime::UNIX_EPOCH);
        assert_eq!(c.now(), SystemTime::UNIX_EPOCH);
        let dynclock: Arc<dyn Clock> = c.clone();
        assert_eq!(dynclock.now(), SystemTime::UNIX_EPOCH);
        assert!(SystemClock.now() > start);
    }
}
