//! SPEC 19.6 framework items (P14): concurrent derivations are deterministic and leave one row;
//! leases only deduplicate (losing or failing to get one never affects correctness).

mod framework_support;

use bytes::Bytes;
use framework_support::*;
use scion_core::{Clock, ErrorCode, FaultAction, points};
use scion_derived::Registry;
use scion_store::{Consistency, DerivedKey};
use std::sync::Arc;
use std::time::Duration;

fn dk(ty: &str, version: u32, key: Bytes) -> DerivedKey {
    DerivedKey {
        ty: ty.to_owned(),
        version,
        key,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_same_key_identical_values_one_row() {
    let (storage, handles) = storage();
    let mk = || {
        let mut t = ChainType::new("test_chain", 1);
        t.delay = Some(Duration::from_millis(2));
        Arc::new(t)
    };
    let (ta, tb) = (mk(), mk());
    let mut ra = Registry::empty();
    ra.register_lazy(ta.clone());
    let mut rb = Registry::empty();
    rb.register_lazy(tb.clone());
    // Two workers (separate engines, as in two server processes) over one store.
    let wa = engine(&storage, ra, "worker-a");
    let wb = engine(&storage, rb, "worker-b");
    let n = 60;
    let key = ck(n);
    let (a, b) = tokio::join!(
        tokio::spawn({
            let wa = wa.clone();
            let key = key.clone();
            async move { wa.get("test_chain", None, &key).await }
        }),
        tokio::spawn({
            let wb = wb.clone();
            let key = key.clone();
            async move { wb.get("test_chain", None, &key).await }
        }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();
    assert_eq!(a, b, "byte-identical values");
    assert_eq!(a, expected_chain(1, n)[n as usize]);
    // Exactly one row per key, and the stored value is the returned one.
    assert_eq!(handles.meta.table_counts().derived, n as usize + 1);
    let row = storage
        .meta
        .get_derived(&dk("test_chain", 1, key), Consistency::Strong)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.value, Some(a));
    // Leases deduplicate most of the work (best effort; not required for correctness).
    let total = ta.derives() + tb.derives();
    assert!(total > n && total <= 2 * (n + 1), "total derives {total}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_queries_one_engine() {
    let (storage, handles) = storage();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(FibType));
    let eng = engine(&storage, reg, "w");
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let eng = eng.clone();
        tasks.push(tokio::spawn(async move {
            eng.get("test_fib", None, &ck(70)).await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap(), ck(fib(70)));
    }
    assert_eq!(handles.meta.table_counts().derived, 71);
}

#[tokio::test]
async fn foreign_unexpired_lease_does_not_block_derivation() {
    let (storage, handles) = storage();
    let clock = clock();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(ChainType::new("test_chain", 1)));
    let eng = engine_with(&storage, reg, "w", clock.clone());
    // A crashed worker left leases (valid for an hour) on the target and an intermediate key.
    for k in [ck(10), ck(5)] {
        assert!(
            storage
                .meta
                .try_lease(
                    &dk("test_chain", 1, k),
                    "crashed",
                    Duration::from_secs(3600),
                    clock.now()
                )
                .await
                .unwrap()
        );
    }
    let v = eng.get("test_chain", None, &ck(10)).await.unwrap();
    assert_eq!(v, expected_chain(1, 10)[10]);
    assert_eq!(handles.meta.table_counts().derived, 11);
}

#[tokio::test]
async fn lease_backend_failures_do_not_affect_correctness() {
    let (storage, _h) = storage();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(ChainType::new("test_chain", 1)));
    let eng = engine(&storage, reg, "w");
    storage.faults.arm(
        &points::meta_before("try_lease"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let v = eng.get("test_chain", None, &ck(7)).await.unwrap();
    assert_eq!(v, expected_chain(1, 7)[7]);
    storage.faults.disarm_all();

    storage.faults.arm(
        &points::meta_before("release_lease"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let v = eng.get("test_chain", None, &ck(12)).await.unwrap();
    assert_eq!(v, expected_chain(1, 12)[12]);
    storage.faults.disarm_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_lost_mid_derivation_both_write_one_row() {
    let (storage, handles) = storage();
    let clock = clock();
    let mk = || {
        let mut t = ChainType::new("test_chain", 1);
        t.delay = Some(Duration::from_millis(300));
        Arc::new(t)
    };
    let (ta, tb) = (mk(), mk());
    let mut ra = Registry::empty();
    ra.register_lazy(ta.clone());
    let mut rb = Registry::empty();
    rb.register_lazy(tb.clone());
    let wa = engine_with(&storage, ra, "a", clock.clone());
    let wb = engine_with(&storage, rb, "b", clock.clone());

    let a = tokio::spawn(async move { wa.get("test_chain", None, &ck(0)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A's lease expires while it is still deriving; B takes the lease over.
    clock.advance(Duration::from_secs(3600));
    let b = tokio::spawn(async move { wb.get("test_chain", None, &ck(0)).await });
    let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
    assert_eq!(ta.derives(), 1);
    assert_eq!(tb.derives(), 1);
    assert_eq!(a, b);
    assert_eq!(a, expected_chain(1, 0)[0]);
    assert_eq!(handles.meta.table_counts().derived, 1);
}
