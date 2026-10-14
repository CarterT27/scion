//! SPEC 19.6 framework items (P14): iterative lazy derivation with write-back, version
//! selection, the generic query path for runtime-registered types.

mod framework_support;

use framework_support::*;
use scion_core::ErrorCode;
use scion_core::test_support::full_mode;
use scion_derived::{DeriveMode, Registry};
use std::sync::Arc;

/// Derive key `n` of a chain on a thread with a 256 KiB stack, starting from a stored value at
/// key `seed`; verify every intermediate value was written back and only the missing keys were
/// derived.
#[allow(clippy::expect_used)] // test helper (not itself a #[test])
fn chain_on_small_stack(n: u64, seed: u64) {
    // The in-memory backend runs on its own runtime (normal stacks) so that only the engine's
    // stack use is measured (see framework_support::offstack).
    let store_rt = tokio::runtime::Runtime::new().expect("runtime");
    let (storage, handles) = offstack_storage(store_rt.handle().clone());
    let ty = Arc::new(ChainType::new("test_chain", 1));
    let mut reg = Registry::empty();
    reg.register_lazy(ty.clone());
    let eng = engine(&storage, reg, "w1");
    let expected = expected_chain(1, n);

    let worker = {
        let eng = eng.clone();
        std::thread::Builder::new()
            .name("small-stack".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(Box::pin(async move {
                    let s = eng.get("test_chain", None, &ck(seed)).await?;
                    let v = eng.get("test_chain", None, &ck(n)).await?;
                    Ok::<_, scion_core::ScionError>((s, v))
                }))
            })
            .expect("spawn")
    };
    let (s, v) = worker.join().expect("no stack overflow").expect("derive");
    assert_eq!(s, expected[seed as usize]);
    assert_eq!(v, expected[n as usize]);
    // Only the keys above the stored seed value were derived by the second query.
    assert_eq!(ty.derives(), n + 1);
    assert_eq!(handles.meta.table_counts().derived, n as usize + 1);

    store_rt.block_on(async {
        for i in 0..=n {
            let got = eng
                .get_stored("test_chain", Some(1), &ck(i))
                .await
                .expect("read");
            assert_eq!(got.as_ref(), Some(&expected[i as usize]), "key {i}");
        }
    });
}

#[test]
fn lazy_chain_is_iterative_and_writes_back() {
    chain_on_small_stack(10_000, 17);
}

#[test]
#[ignore]
fn full_lazy_chain_1e5_iterative() {
    if !full_mode() {
        eprintln!("SKIP: SCION_FULL unset");
        return;
    }
    chain_on_small_stack(100_000, 3);
}

#[tokio::test]
async fn lazy_starts_from_nearest_stored_value() {
    let (storage, _h) = storage();
    let ty = Arc::new(ChainType::new("test_chain", 1));
    let mut reg = Registry::empty();
    reg.register_lazy(ty.clone());
    let eng = engine(&storage, reg, "w");
    let expected = expected_chain(1, 300);
    assert_eq!(
        eng.get("test_chain", None, &ck(200)).await.unwrap(),
        expected[200]
    );
    assert_eq!(ty.derives(), 201);
    assert_eq!(
        eng.get("test_chain", None, &ck(300)).await.unwrap(),
        expected[300]
    );
    assert_eq!(ty.derives(), 301);
    // A stored value is returned without any derivation or dependency walk.
    let deps_before = ty.deps_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        eng.get("test_chain", None, &ck(150)).await.unwrap(),
        expected[150]
    );
    assert_eq!(ty.derives(), 301);
    assert_eq!(
        ty.deps_calls.load(std::sync::atomic::Ordering::SeqCst),
        deps_before
    );
}

#[tokio::test]
async fn lazy_dag_dependencies_are_derived_once() {
    let (storage, handles) = storage();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(FibType));
    let eng = engine(&storage, reg, "w");
    let v = eng.get("test_fib", None, &ck(90)).await.unwrap();
    assert_eq!(v, ck(fib(90)));
    assert_eq!(handles.meta.table_counts().derived, 91);
    for i in 0..=90 {
        assert_eq!(
            eng.get_stored("test_fib", None, &ck(i)).await.unwrap(),
            Some(ck(fib(i)))
        );
    }
}

#[tokio::test]
async fn dependency_cycle_is_an_error_not_a_hang() {
    let (storage, handles) = storage();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(CycleType));
    let eng = engine(&storage, reg, "w");
    let err = eng.get("test_cycle", None, &ck(0)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(handles.meta.table_counts().derived, 0);
}

#[tokio::test]
async fn version_bump_rederives_and_old_values_stay_readable() {
    let (storage, _h) = storage();
    let key = ck(40);
    let v1 = expected_chain(1, 40);
    let v2 = expected_chain(2, 40);
    assert_ne!(v1[40], v2[40]);

    // Old binary: only v1 registered.
    let mut reg1 = Registry::empty();
    reg1.register_lazy(Arc::new(ChainType::new("test_chain", 1)));
    let old = engine(&storage, reg1, "old");
    assert_eq!(old.get("test_chain", None, &key).await.unwrap(), v1[40]);

    // New server: v2 registered (v1 no longer registered); a query without a version
    // re-derives at v2.
    let t2 = Arc::new(ChainType::new("test_chain", 2));
    let mut reg2 = Registry::empty();
    reg2.register_lazy(t2.clone());
    let new = engine(&storage, reg2, "new");
    assert_eq!(new.registry().latest_version("test_chain"), Some(2));
    assert_eq!(
        new.get_stored("test_chain", None, &key).await.unwrap(),
        None
    );
    assert_eq!(new.get("test_chain", None, &key).await.unwrap(), v2[40]);
    assert_eq!(t2.derives(), 41);
    // Old version values remain readable, by explicit version.
    assert_eq!(new.get("test_chain", Some(1), &key).await.unwrap(), v1[40]);
    assert_eq!(
        new.get_stored("test_chain", Some(1), &key).await.unwrap(),
        Some(v1[40].clone())
    );
    // An old version that is not registered cannot be derived: NOT_FOUND for absent keys.
    let err = new.get("test_chain", Some(1), &ck(41)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    // Both versions registered: None selects the latest; v1 is still derivable.
    let mut reg3 = Registry::empty();
    reg3.register_lazy(Arc::new(ChainType::new("test_chain", 2)));
    reg3.register_lazy(Arc::new(ChainType::new("test_chain", 1)));
    let both = engine(&storage, reg3, "both");
    assert_eq!(both.get("test_chain", None, &key).await.unwrap(), v2[40]);
    let v1_41 = expected_chain(1, 41);
    assert_eq!(
        both.get("test_chain", Some(1), &ck(41)).await.unwrap(),
        v1_41[41]
    );
}

#[tokio::test]
async fn runtime_registered_type_is_queryable_through_generic_path() {
    let (storage, _h) = storage();
    // The v1 registry plus a test-only type registered at runtime: no other change.
    let mut reg = Registry::v1_default();
    reg.register_lazy(Arc::new(ChainType::new("test_only", 1)));
    let eng = engine(&storage, reg, "w");
    let expected = expected_chain(1, 5);
    assert_eq!(
        eng.get("test_only", None, &ck(5)).await.unwrap(),
        expected[5]
    );
    assert_eq!(
        eng.get("test_only", Some(1), &ck(5)).await.unwrap(),
        expected[5]
    );
    assert_eq!(eng.registry().latest_version("test_only"), Some(1));

    // Unknown types are NOT_FOUND on both paths.
    let err = eng.get("no_such_type", None, b"k").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    let err = eng
        .get_stored("no_such_type", None, b"k")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    // A registered non-lazy type without a stored value is NOT_FOUND (never derived on query).
    let err = eng.get("commit_graph", None, b"k").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn stored_eager_values_are_served_by_the_generic_path() {
    let (storage, _h) = storage();
    let mut reg = Registry::empty();
    reg.register_eager(Arc::new(TagEager {
        name: "test_eager",
        version: 1,
        extra: 0,
        content_bearing: false,
    }));
    let eng = engine(&storage, reg, "w");
    storage
        .meta
        .put_derived(vec![scion_store::DerivedRow {
            key: scion_store::DerivedKey {
                ty: "test_eager".into(),
                version: 1,
                key: bytes::Bytes::from_static(b"k"),
            },
            value: Some(bytes::Bytes::from_static(b"v")),
            storage_key: None,
            source_object: None,
            at: std::time::SystemTime::UNIX_EPOCH,
        }])
        .await
        .unwrap();
    assert_eq!(
        eng.get("test_eager", None, b"k").await.unwrap().as_ref(),
        b"v"
    );
    let err = eng.get("test_eager", None, b"missing").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn metrics_report_durations_and_balanced_backlog() {
    let (storage, _h) = storage();
    let mut reg = Registry::empty();
    reg.register_lazy(Arc::new(ChainType::new("test_chain", 1)));
    reg.register_lazy(Arc::new(CycleType));
    let eng = engine(&storage, reg, "w");
    let m = Arc::new(RecMetrics::default());
    eng.set_metrics(m.clone());
    eng.get("test_chain", None, &ck(9)).await.unwrap();
    assert_eq!(m.durations_of("test_chain", DeriveMode::Lazy), 10);
    assert_eq!(m.backlog_peak("test_chain"), 10);
    assert_eq!(m.backlog_sum("test_chain"), 0);
    // A failed derivation still returns the backlog gauge to zero.
    assert!(eng.get("test_cycle", None, &ck(1)).await.is_err());
    assert_eq!(m.backlog_sum("test_cycle"), 0);
}
