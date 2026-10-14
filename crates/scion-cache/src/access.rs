//! Per-principal access decisions (SPEC 10.4, 18.6; PLAN 6.13; P10).

use parking_lot::Mutex;
use scion_core::ObjectId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum map size before expired entries are swept on insert.
const SWEEP_MIN: usize = 1024;

/// All cached state, behind one lock so an epoch change clears it atomically.
#[derive(Debug, Default)]
struct State {
    /// `(principal, object) -> (allowed, recorded_at)`.
    decisions: HashMap<(String, ObjectId), (bool, Instant)>,
    /// `principal -> (is_root_reader, recorded_at)`.
    root_readers: HashMap<String, (bool, Instant)>,
    /// The last policy epoch seen; `None` before the first.
    epoch: Option<u64>,
    /// Decision-map size that triggers the next sweep of expired entries.
    sweep_at: usize,
}

/// Decisions keyed by `(principal, object)` with TTL plus a per-principal root-reader flag
/// (SPEC 10.4; PLAN 6.13). Decisions are never shared across principals and are cleared on a
/// policy epoch change.
pub struct AccessCache {
    ttl: Duration,
    state: Mutex<State>,
}

impl AccessCache {
    /// A cache with the given TTL (`access_cache_ttl_ms`, SPEC 10.4). A zero TTL caches
    /// nothing usable.
    pub fn new(ttl: Duration) -> Self {
        AccessCache {
            ttl,
            state: Mutex::new(State {
                sweep_at: SWEEP_MIN,
                ..State::default()
            }),
        }
    }

    fn fresh(&self, at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(at) < self.ttl
    }

    /// A fresh decision for `(principal, id)` recorded less than `ttl` before `now`
    /// (SPEC 10.4: cached for at most `access_cache_ttl_ms`). Only `principal`'s own decision
    /// is ever returned.
    pub fn get(&self, principal: &str, id: &ObjectId, now: Instant) -> Option<bool> {
        let mut st = self.state.lock();
        let key = (principal.to_owned(), *id);
        let (allowed, at) = *st.decisions.get(&key)?;
        if self.fresh(at, now) {
            Some(allowed)
        } else {
            st.decisions.remove(&key);
            None
        }
    }

    /// Record a decision for `(principal, id)` made at `now`.
    pub fn put(&self, principal: &str, id: &ObjectId, allowed: bool, now: Instant) {
        let mut st = self.state.lock();
        st.decisions
            .insert((principal.to_owned(), *id), (allowed, now));
        if st.decisions.len() >= st.sweep_at {
            let ttl = self.ttl;
            st.decisions
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < ttl);
            st.root_readers
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < ttl);
            st.sweep_at = (st.decisions.len() * 2).max(SWEEP_MIN);
        }
    }

    /// A fresh root-reader flag for `principal` (PLAN 6.13: confirmed by a positive
    /// CheckAccess on a root-class object, cached for `access_cache_ttl_ms`).
    pub fn root_reader(&self, principal: &str, now: Instant) -> Option<bool> {
        let mut st = self.state.lock();
        let (v, at) = *st.root_readers.get(principal)?;
        if self.fresh(at, now) {
            Some(v)
        } else {
            st.root_readers.remove(principal);
            None
        }
    }

    /// Record the root-reader flag for `principal` at `now`.
    pub fn set_root_reader(&self, principal: &str, v: bool, now: Instant) {
        self.state
            .lock()
            .root_readers
            .insert(principal.to_owned(), (v, now));
    }

    /// Clear everything when `epoch` differs from the last seen policy epoch (SPEC 10.4: all
    /// decision caches are dropped when a response carries a different `scion-policy-epoch`).
    /// The first epoch observed also clears decisions recorded before any epoch was known.
    pub fn on_policy_epoch(&self, epoch: u64) {
        let mut st = self.state.lock();
        if st.epoch != Some(epoch) {
            if st.epoch.is_some() || !st.decisions.is_empty() || !st.root_readers.is_empty() {
                tracing::debug!(old = ?st.epoch, new = epoch, "access cache cleared on policy epoch change");
            }
            st.decisions.clear();
            st.root_readers.clear();
            st.sweep_at = SWEEP_MIN;
            st.epoch = Some(epoch);
        }
    }

    /// The last policy epoch seen, if any.
    pub fn policy_epoch(&self) -> Option<u64> {
        self.state.lock().epoch
    }

    /// Number of cached `(principal, object)` decisions, fresh or not yet swept.
    pub fn len(&self) -> usize {
        self.state.lock().decisions.len()
    }

    /// True when no decision is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_keeps_fresh_entries() {
        let c = AccessCache::new(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..(SWEEP_MIN as u32 - 1) {
            c.put("github:a", &ObjectId::hash_blob(&i.to_le_bytes()), true, t0);
        }
        let later = t0 + Duration::from_secs(20);
        let fresh = ObjectId::hash_blob(b"fresh");
        c.put("github:a", &fresh, false, later);
        // The sweep removed every expired decision.
        assert_eq!(c.len(), 1);
        assert_eq!(c.get("github:a", &fresh, later), Some(false));
    }
}
