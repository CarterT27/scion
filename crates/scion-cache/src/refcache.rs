//! Ref TTL cache with monotonic versions (SPEC 10.3; P10).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::Mutex;
use scion_core::ObjectId;
use scion_object::RefName;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Caches `(target, version)` per ref for `ttl`; never returns a lower version than one it
/// returned before.
pub struct RefCache {
    ttl: Duration,
    entries: Mutex<HashMap<RefName, (ObjectId, u64, Instant)>>,
}

impl RefCache {
    /// A cache with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        RefCache {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A fresh entry, if any.
    pub fn get(&self, name: &RefName, now: Instant) -> Option<(ObjectId, u64)> {
        todo!("P10")
    }

    /// Record an upstream answer; returns the value to serve (the higher version wins).
    pub fn observe(
        &self,
        name: &RefName,
        target: ObjectId,
        version: u64,
        now: Instant,
    ) -> (ObjectId, u64) {
        todo!("P10")
    }
}
