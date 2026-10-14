//! SPEC 19.2 identical CreateChangeset requests (sequential and 50-way concurrent) and
//! PLAN 6.5 bootstrap idempotency.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod create_support;

use create_support::*;
use scion_core::{ChangeId, Kind, ObjectId, Principal};
use scion_history::{AllowAll, HistoryConfig};
use scion_object::{Canonical, Changeset, RefName};
use std::sync::Arc;

#[tokio::test]
async fn sequential_identical_requests_return_one_id() {
    let e = env().await;
    let delta = vec![put(&e.h, "a/b", "1").await, put(&e.h, "c", "2").await];
    let before = e.mem.meta.table_counts();
    let first = create(&e.h, vec![e.root], delta.clone()).await;
    for _ in 0..5 {
        let again = create(&e.h, vec![e.root], delta.clone()).await;
        assert_eq!(again, first);
    }
    let after = e.mem.meta.table_counts();
    assert_eq!(after.changesets, before.changesets + 1);
    assert_fsck_clean(&e.h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_identical_requests_return_one_id() {
    let e = Arc::new(env().await);
    let delta = vec![
        put(&e.h, "a/y/z", "deep").await,
        put(&e.h, "top", "t").await,
    ];
    let before = e.mem.meta.table_counts();
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let e = e.clone();
        let delta = delta.clone();
        tasks.push(tokio::spawn(async move {
            e.h.create_changeset(req(vec![e.root], delta), &alice(), &AllowAll)
                .await
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    for t in tasks {
        let created = t.await.unwrap().expect("identical requests never error");
        ids.insert(created.changeset_id);
    }
    assert_eq!(ids.len(), 1);
    let after = e.mem.meta.table_counts();
    assert_eq!(after.changesets, before.changesets + 1);
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn bootstrap_is_idempotent() {
    let (h, mem, _clock) = history_with(HistoryConfig::default());
    let root = h.bootstrap().await.unwrap();
    let counts = mem.meta.table_counts();
    assert_eq!(counts.changesets, 1);
    assert_eq!(counts.refs, 1);
    for _ in 0..3 {
        assert_eq!(h.bootstrap().await.unwrap(), root);
    }
    let again = mem.meta.table_counts();
    assert_eq!(again.changesets, 1);
    assert_eq!(again.refs, 1);
    assert_eq!(again.ref_log, 1);

    // Ref main at version 1.
    let main = h.meta().get_ref(&RefName::main()).await.unwrap().unwrap();
    assert_eq!(main.target, root);
    assert_eq!(main.version, 1);

    // PLAN 6.5 root changeset.
    let rec = h.changeset(&root).await.unwrap();
    assert!(rec.changeset.parents.is_empty());
    assert!(rec.changeset.delta.is_empty());
    assert_eq!(rec.changeset.root_tree, scion_core::empty_tree_id());
    assert_eq!(rec.generation, 1);
    assert!(rec.skip.is_empty());
    assert_eq!(rec.created_by, "service:scion");
    let digest = blake3::hash(b"scion-root-v1");
    let mut cid = [0u8; 16];
    cid.copy_from_slice(&digest.as_bytes()[..16]);
    assert_eq!(rec.changeset.change_id, ChangeId(cid));
    let meta = h.objects().get_meta(&rec.changeset.meta).await.unwrap();
    assert_eq!(meta.author, b"scion <scion@localhost>".to_vec());
    assert_eq!(meta.author_time.seconds, 0);
    assert_eq!(meta.author_time.tz_offset_minutes, 0);
    assert_eq!(meta.committer, None);
    assert_eq!(meta.committer_time, None);
    assert_eq!(meta.creator, b"service:scion".to_vec());
    assert_eq!(meta.message, b"root".to_vec());
    // The root tree object exists (SPEC 4.5 invariant 2).
    assert!(
        h.objects()
            .stat(&scion_core::empty_tree_id())
            .await
            .unwrap()
            .is_some()
    );
    assert_fsck_clean(&h).await;

    // The id is deterministic across repositories.
    let (h2, _, _) = history_with(HistoryConfig::default());
    assert_eq!(h2.bootstrap().await.unwrap(), root);
    let encoded = Changeset::decode(&h.objects().get(&root).await.unwrap()).unwrap();
    assert_eq!(
        ObjectId::hash_encoded(Kind::Changeset, &encoded.encode()),
        root
    );
}

#[tokio::test]
async fn bootstrap_keeps_an_existing_main() {
    let (h, _mem, _clock) = history_with(HistoryConfig::default());
    let root = h.bootstrap().await.unwrap();
    let c = create(&h, vec![root], vec![put(&h, "a", "a").await]).await;
    // Move main forward, then re-run bootstrap: main is untouched.
    let cas = scion_store::RefCas {
        name: RefName::main(),
        expect: scion_store::RefExpect::Version(1),
        new_target: Some(c.changeset_id),
        principal: "github:alice".to_owned(),
        at: std::time::SystemTime::UNIX_EPOCH,
        landed: Vec::new(),
        audit: None,
    };
    h.meta().cas_ref(cas).await.unwrap();
    assert_eq!(h.bootstrap().await.unwrap(), root);
    let main = h.meta().get_ref(&RefName::main()).await.unwrap().unwrap();
    assert_eq!(main.target, c.changeset_id);
    assert_eq!(main.version, 2);
}

#[tokio::test]
async fn bootstrap_principal_is_configurable() {
    let cfg = HistoryConfig {
        bootstrap_principal: Principal::Service("custom".to_owned()),
        ..HistoryConfig::default()
    };
    let (h, _, _) = history_with(cfg);
    let root = h.bootstrap().await.unwrap();
    let rec = h.changeset(&root).await.unwrap();
    let meta = h.objects().get_meta(&rec.changeset.meta).await.unwrap();
    assert_eq!(meta.creator, b"service:custom".to_vec());
}
