//! Local redaction set of a daemon (SPEC 10.2, 13.3, 18.8; P10).

use parking_lot::Mutex;
use scion_core::ObjectId;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Redacted ids, the local redaction epoch and the last successful refresh (SPEC 13.3).
///
/// The epoch never decreases. Staleness drives the SPEC 13.3 fail-closed rule: when the last
/// successful refresh is older than `redaction_max_staleness_ms`, the daemon stops serving
/// from cache.
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

    /// Apply redacted ids observed at `epoch` (an incremental `ListRedactions` answer, SPEC
    /// 18.8). Adds the ids, raises the epoch to `epoch` if higher, and counts as a successful
    /// refresh at `now`.
    pub fn apply(&self, ids: &[ObjectId], epoch: u64, now: Instant) {
        let mut st = self.state.lock();
        st.0.extend(ids.iter().copied());
        st.1 = st.1.max(epoch);
        st.2 = st.2.max(now);
    }

    /// Replace the whole set with a complete listing (`ListRedactions(since_epoch = 0)`) taken
    /// at `epoch`, so unredactions propagate (storage-semantics consumer rule). Counts as a
    /// successful refresh at `now`. A listing older than the local epoch is ignored, apart from
    /// the refresh time.
    pub fn replace(&self, ids: &[ObjectId], epoch: u64, now: Instant) {
        let mut st = self.state.lock();
        if epoch >= st.1 {
            st.0 = ids.iter().copied().collect();
            st.1 = epoch;
        }
        st.2 = st.2.max(now);
    }

    /// Record a successful refresh at `epoch` (SPEC 18.8 `last_refresh = now`).
    pub fn mark_refreshed(&self, epoch: u64, now: Instant) {
        let mut st = self.state.lock();
        st.1 = st.1.max(epoch);
        st.2 = st.2.max(now);
    }

    /// True when `id` is redacted (SPEC 13.3: checked before serving any cached object).
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.state.lock().0.contains(id)
    }

    /// The local redaction epoch.
    pub fn epoch(&self) -> u64 {
        self.state.lock().1
    }

    /// The time of the last successful refresh.
    pub fn last_refresh(&self) -> Instant {
        self.state.lock().2
    }

    /// Number of redacted ids known locally.
    pub fn len(&self) -> usize {
        self.state.lock().0.len()
    }

    /// True when no id is known to be redacted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when the last refresh is older than `max` (the SPEC 13.3 fail-closed trigger).
    pub fn is_stale(&self, now: Instant, max: Duration) -> bool {
        now.saturating_duration_since(self.state.lock().2) > max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId::hash_blob(&[n])
    }

    #[test]
    fn apply_contains_epoch_and_staleness() {
        let t0 = Instant::now();
        let r = LocalRedactions::new(t0);
        assert_eq!(r.epoch(), 0);
        assert!(r.is_empty());
        assert!(!r.is_stale(t0 + Duration::from_secs(10), Duration::from_secs(10)));
        assert!(r.is_stale(t0 + Duration::from_secs(11), Duration::from_secs(10)));

        r.apply(&[id(1), id(2)], 3, t0 + Duration::from_secs(5));
        assert!(r.contains(&id(1)) && r.contains(&id(2)) && !r.contains(&id(3)));
        assert_eq!(r.epoch(), 3);
        assert!(!r.is_stale(t0 + Duration::from_secs(15), Duration::from_secs(10)));
        assert!(r.is_stale(t0 + Duration::from_secs(16), Duration::from_secs(10)));

        // Epoch never goes backwards.
        r.apply(&[id(3)], 2, t0 + Duration::from_secs(6));
        assert_eq!(r.epoch(), 3);
        assert!(r.contains(&id(3)));
        r.mark_refreshed(1, t0 + Duration::from_secs(20));
        assert_eq!(r.epoch(), 3);
        assert_eq!(r.last_refresh(), t0 + Duration::from_secs(20));
        r.mark_refreshed(9, t0 + Duration::from_secs(21));
        assert_eq!(r.epoch(), 9);
        // Refresh time never goes backwards either.
        r.mark_refreshed(9, t0);
        assert_eq!(r.last_refresh(), t0 + Duration::from_secs(21));
    }

    #[test]
    fn replace_drops_unredacted_ids() {
        let t0 = Instant::now();
        let r = LocalRedactions::new(t0);
        r.apply(&[id(1), id(2)], 2, t0);
        r.replace(&[id(2)], 3, t0);
        assert!(!r.contains(&id(1)));
        assert!(r.contains(&id(2)));
        assert_eq!(r.epoch(), 3);
        // An older complete listing does not roll the set back.
        r.replace(&[id(1)], 1, t0);
        assert!(!r.contains(&id(1)));
        assert_eq!(r.len(), 1);
    }
}
