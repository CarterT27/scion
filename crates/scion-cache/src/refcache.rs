//! Ref TTL cache with monotonic versions (SPEC 10.3, 16.1 class 1; P10).

use parking_lot::Mutex;
use scion_core::ObjectId;
use scion_object::RefName;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Caches `(target, version)` per ref for `ttl`; never returns a lower version than one it
/// returned before (SPEC 10.3 monotonic reads).
///
/// Entries are kept after their TTL expires (they are just not served by [`RefCache::get`]),
/// so that the highest version ever served still bounds later upstream answers.
pub struct RefCache {
    ttl: Duration,
    entries: Mutex<HashMap<RefName, (ObjectId, u64, Instant)>>,
}

impl RefCache {
    /// A cache with the given TTL (`ref_cache_ttl_ms`, SPEC 10.3). A zero TTL never serves
    /// from cache but still enforces monotonic versions.
    pub fn new(ttl: Duration) -> Self {
        RefCache {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// A fresh entry, if any: one confirmed by upstream less than `ttl` before `now`
    /// (SPEC 10.3: cached for at most `ref_cache_ttl_ms`).
    pub fn get(&self, name: &RefName, now: Instant) -> Option<(ObjectId, u64)> {
        let entries = self.entries.lock();
        let (target, version, at) = entries.get(name)?;
        (now.saturating_duration_since(*at) < self.ttl).then_some((*target, *version))
    }

    /// Record an upstream answer; returns the value to serve (the higher version wins, SPEC
    /// 10.3). An answer older than what was already served (out-of-order or lagging upstream)
    /// is ignored and does not extend freshness; an equal or higher version refreshes the
    /// entry at `now`.
    pub fn observe(
        &self,
        name: &RefName,
        target: ObjectId,
        version: u64,
        now: Instant,
    ) -> (ObjectId, u64) {
        let mut entries = self.entries.lock();
        match entries.get_mut(name) {
            Some((cur_target, cur_version, _)) if *cur_version > version => {
                tracing::debug!(
                    ref_name = %name,
                    observed = version,
                    served = *cur_version,
                    "ref cache: ignoring a lower upstream version"
                );
                (*cur_target, *cur_version)
            }
            Some(entry) => {
                *entry = (target, version, now);
                (target, version)
            }
            None => {
                entries.insert(name.clone(), (target, version, now));
                (target, version)
            }
        }
    }
}
