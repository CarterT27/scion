//! Local redaction set of a daemon (SPEC 10.2, 13.3; P10).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::Mutex;
use scion_core::ObjectId;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Redacted ids, the local redaction epoch and the last successful refresh.
pub struct LocalRedactions {
    state: Mutex<(HashSet<ObjectId>, u64, Instant)>,
}

impl LocalRedactions {
    /// Empty, refreshed at `now`.
    pub fn new(now: Instant) -> Self {
        LocalRedactions {
            state: Mutex::new((HashSet::new(), 0, now)),
        }
    }

    /// Apply redacted ids observed at `epoch`.
    pub fn apply(&self, ids: &[ObjectId], epoch: u64, now: Instant) {
        todo!("P10")
    }

    /// Record a successful refresh at `epoch`.
    pub fn mark_refreshed(&self, epoch: u64, now: Instant) {
        todo!("P10")
    }

    /// True when `id` is redacted.
    pub fn contains(&self, id: &ObjectId) -> bool {
        todo!("P10")
    }

    /// The local redaction epoch.
    pub fn epoch(&self) -> u64 {
        todo!("P10")
    }

    /// True when the last refresh is older than `max` (fail-closed trigger).
    pub fn is_stale(&self, now: Instant, max: Duration) -> bool {
        todo!("P10")
    }
}
