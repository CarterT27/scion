//! SPEC 19.9 / 10.3 / 10.4 / 13.3: ref cache, access cache and local redactions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use proptest::prelude::*;
use scion_cache::{AccessCache, LocalRedactions, RefCache};
use scion_core::ObjectId;
use scion_object::RefName;
use std::time::{Duration, Instant};

fn oid(n: u64) -> ObjectId {
    ObjectId::hash_encoded(scion_core::Kind::Changeset, &n.to_le_bytes())
}

#[test]
fn ref_ttl_is_honored() {
    let ttl = Duration::from_millis(500);
    let c = RefCache::new(ttl);
    let main = RefName::main();
    let t0 = Instant::now();
    assert_eq!(c.get(&main, t0), None);
    assert_eq!(c.observe(&main, oid(1), 1, t0), (oid(1), 1));
    assert_eq!(c.get(&main, t0), Some((oid(1), 1)));
    assert_eq!(
        c.get(&main, t0 + Duration::from_millis(499)),
        Some((oid(1), 1))
    );
    assert_eq!(c.get(&main, t0 + ttl), None, "expired at ttl");
    assert_eq!(c.get(&main, t0 + Duration::from_secs(60)), None);
    // A fresh upstream answer revives it.
    let t1 = t0 + Duration::from_secs(60);
    assert_eq!(c.observe(&main, oid(2), 2, t1), (oid(2), 2));
    assert_eq!(
        c.get(&main, t1 + Duration::from_millis(100)),
        Some((oid(2), 2))
    );
}

#[test]
fn ref_zero_ttl_never_serves_but_stays_monotonic() {
    let c = RefCache::new(Duration::ZERO);
    let main = RefName::main();
    let t0 = Instant::now();
    c.observe(&main, oid(5), 5, t0);
    assert_eq!(c.get(&main, t0), None);
    assert_eq!(c.observe(&main, oid(3), 3, t0), (oid(5), 5));
}

#[test]
fn ref_versions_stay_monotonic_across_expiry_and_reordering() {
    let ttl = Duration::from_millis(100);
    let c = RefCache::new(ttl);
    let main = RefName::main();
    let t0 = Instant::now();
    assert_eq!(c.observe(&main, oid(7), 7, t0), (oid(7), 7));
    // TTL expires; upstream answers out of order with older versions.
    let t1 = t0 + Duration::from_secs(1);
    assert_eq!(c.get(&main, t1), None);
    assert_eq!(c.observe(&main, oid(5), 5, t1), (oid(7), 7));
    assert_eq!(c.observe(&main, oid(6), 6, t1), (oid(7), 7));
    // A stale answer does not extend freshness of the old entry.
    assert_eq!(c.get(&main, t1), None);
    // Equal version refreshes.
    assert_eq!(c.observe(&main, oid(7), 7, t1), (oid(7), 7));
    assert_eq!(c.get(&main, t1), Some((oid(7), 7)));
    // Newer wins.
    assert_eq!(c.observe(&main, oid(9), 9, t1), (oid(9), 9));
    assert_eq!(c.observe(&main, oid(8), 8, t1), (oid(9), 9));
    assert_eq!(c.get(&main, t1), Some((oid(9), 9)));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(scion_core::test_support::proptest_cases(256)))]

    /// Whatever the order of upstream answers and reads, the served version never decreases,
    /// a served version is always one upstream reported, and fresh hits respect the TTL.
    #[test]
    fn ref_served_versions_never_decrease(
        ops in proptest::collection::vec((any::<bool>(), 0u64..50, 0u64..400), 1..200)
    ) {
        let ttl = Duration::from_millis(100);
        let c = RefCache::new(ttl);
        let main = RefName::main();
        let t0 = Instant::now();
        let mut now = t0;
        let mut served_max = 0u64;
        let mut last_refresh: Option<Instant> = None;
        let mut observed = std::collections::HashSet::new();
        for (is_read, version, advance_ms) in ops {
            now += Duration::from_millis(advance_ms);
            let served = if is_read {
                match c.get(&main, now) {
                    Some(v) => {
                        let at = last_refresh.expect("a hit implies an observation");
                        prop_assert!(now.duration_since(at) < ttl);
                        Some(v)
                    }
                    None => None,
                }
            } else {
                observed.insert(version);
                let r = c.observe(&main, oid(version), version, now);
                if version >= served_max {
                    last_refresh = Some(now);
                }
                Some(r)
            };
            if let Some((target, v)) = served {
                prop_assert!(v >= served_max, "served {v} after {served_max}");
                prop_assert_eq!(target, oid(v));
                prop_assert!(observed.contains(&v));
                served_max = v;
            }
        }
    }
}

#[test]
fn access_cache_isolates_principals() {
    let c = AccessCache::new(Duration::from_secs(30));
    let t0 = Instant::now();
    let obj = oid(1);
    c.put("github:alice", &obj, true, t0);
    assert_eq!(c.get("github:alice", &obj, t0), Some(true));
    assert_eq!(
        c.get("github:bob", &obj, t0),
        None,
        "never another principal's decision"
    );
    assert_eq!(c.get("github:alice", &oid(2), t0), None);
    c.put("github:bob", &obj, false, t0);
    assert_eq!(c.get("github:bob", &obj, t0), Some(false));
    assert_eq!(c.get("github:alice", &obj, t0), Some(true));

    c.set_root_reader("github:alice", true, t0);
    assert_eq!(c.root_reader("github:alice", t0), Some(true));
    assert_eq!(c.root_reader("github:bob", t0), None);
    c.set_root_reader("github:bob", false, t0);
    assert_eq!(c.root_reader("github:bob", t0), Some(false));
    assert_eq!(c.root_reader("github:alice", t0), Some(true));
    // Look-alike principal strings do not collide.
    assert_eq!(c.get("github:alice ", &obj, t0), None);
    assert_eq!(c.get("service:alice", &obj, t0), None);
}

#[test]
fn access_cache_ttl() {
    let ttl = Duration::from_secs(10);
    let c = AccessCache::new(ttl);
    let t0 = Instant::now();
    c.put("github:a", &oid(1), true, t0);
    c.set_root_reader("github:a", true, t0);
    let almost = t0 + Duration::from_millis(9_999);
    assert_eq!(c.get("github:a", &oid(1), almost), Some(true));
    assert_eq!(c.root_reader("github:a", almost), Some(true));
    assert_eq!(c.get("github:a", &oid(1), t0 + ttl), None);
    assert_eq!(c.root_reader("github:a", t0 + ttl), None);
    // Expired entries stay gone.
    assert_eq!(c.get("github:a", &oid(1), t0), None);
}

#[test]
fn access_cache_clears_on_epoch_change() {
    let c = AccessCache::new(Duration::from_secs(30));
    let t0 = Instant::now();
    c.on_policy_epoch(3);
    assert_eq!(c.policy_epoch(), Some(3));
    c.put("github:a", &oid(1), true, t0);
    c.put("github:b", &oid(1), false, t0);
    c.set_root_reader("github:a", true, t0);
    // Same epoch: nothing changes.
    c.on_policy_epoch(3);
    assert_eq!(c.get("github:a", &oid(1), t0), Some(true));
    assert_eq!(c.root_reader("github:a", t0), Some(true));
    // Different epoch: everything is dropped.
    c.on_policy_epoch(4);
    assert_eq!(c.policy_epoch(), Some(4));
    assert_eq!(c.get("github:a", &oid(1), t0), None);
    assert_eq!(c.get("github:b", &oid(1), t0), None);
    assert_eq!(c.root_reader("github:a", t0), None);
    assert!(c.is_empty());
    // A different epoch in either direction clears.
    c.put("github:a", &oid(1), true, t0);
    c.on_policy_epoch(2);
    assert_eq!(c.get("github:a", &oid(1), t0), None);
}

#[test]
fn local_redactions_staleness_and_epoch() {
    let t0 = Instant::now();
    let r = LocalRedactions::new(t0);
    let max = Duration::from_secs(60);
    assert!(!r.is_stale(t0 + max, max));
    assert!(r.is_stale(t0 + max + Duration::from_millis(1), max));
    r.apply(&[oid(1)], 4, t0 + Duration::from_secs(30));
    assert!(r.contains(&oid(1)));
    assert_eq!(r.epoch(), 4);
    assert!(!r.is_stale(t0 + Duration::from_secs(90), max));
    r.mark_refreshed(5, t0 + Duration::from_secs(100));
    assert_eq!(r.epoch(), 5);
    assert!(!r.is_stale(t0 + Duration::from_secs(160), max));
    assert!(r.is_stale(t0 + Duration::from_secs(161), max));
}
