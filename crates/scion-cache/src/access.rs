//! Per-principal access decisions (SPEC 10.4; PLAN 6.13; P10).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::Mutex;
use scion_core::ObjectId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Decisions keyed by `(principal, object)` with TTL plus a per-principal root-reader flag.
/// Decisions are never shared across principals and are cleared on a policy epoch change.
pub struct AccessCache {
    ttl: Duration,
    decisions: Mutex<HashMap<(String, ObjectId), (bool, Instant)>>,
    root_readers: Mutex<HashMap<String, (bool, Instant)>>,
    epoch: Mutex<Option<u64>>,
}

impl AccessCache {
    /// A cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        AccessCache {
            ttl,
            decisions: Mutex::new(HashMap::new()),
            root_readers: Mutex::new(HashMap::new()),
            epoch: Mutex::new(None),
        }
    }

    /// A fresh decision for `(principal, id)`.
    pub fn get(&self, principal: &str, id: &ObjectId, now: Instant) -> Option<bool> {
        todo!("P10")
    }

    /// Record a decision.
    pub fn put(&self, principal: &str, id: &ObjectId, allowed: bool, now: Instant) {
        todo!("P10")
    }

    /// A fresh root-reader flag for `principal`.
    pub fn root_reader(&self, principal: &str, now: Instant) -> Option<bool> {
        todo!("P10")
    }

    /// Record the root-reader flag.
    pub fn set_root_reader(&self, principal: &str, v: bool, now: Instant) {
        todo!("P10")
    }

    /// Clear everything when `epoch` differs from the last seen policy epoch.
    pub fn on_policy_epoch(&self, epoch: u64) {
        todo!("P10")
    }
}
