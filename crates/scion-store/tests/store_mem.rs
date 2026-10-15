//! MemMetaStore model behaviour (PLAN 3.2): forced transaction failures, stale snapshots,
//! atomic redaction epochs, and the instrumented decorators' fault points, counters and lease
//! spans (SPEC 19.0).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

mod store_support;

use bytes::Bytes;
use scion_core::{
    ChangeId, ErrorCode, FaultAction, FaultRegistry, Kind, ObjectId, Result, subcode,
};
use scion_object::{RefName, RepoPath};
use scion_store::{
    AuditRow, CasOutcome, ChangesetBundle, Consistency, Counters, DerivedKey, DerivedRow,
    InstrumentedMeta, LandedEntry, MemMetaStore, MemOptions, MetaStore, ObjectPathRow, ObjectRow,
    ObjectState, RedactTxn, RefCas, RefExpect, Storage, TableCounts,
};
use std::sync::Arc;
use std::time::Duration;
use store_support::{Capture, opaque_cs_row, t, with_capture};

fn blob_row(n: u32) -> ObjectRow {
    let content = format!("content {n}");
    ObjectRow {
        id: ObjectId::hash_blob(content.as_bytes()),
        kind: Kind::Blob,
        size: content.len() as u64,
        stored_size: 40,
        inline_data: None,
        state: ObjectState::Live,
        created_at: t(1),
    }
}

fn audit(action: &str) -> AuditRow {
    AuditRow {
        at: t(2),
        actor: "github:admin".to_owned(),
        action: action.to_owned(),
        target: "x".to_owned(),
        reason: "r".to_owned(),
        detail: String::new(),
    }
}

/// A fixed workload exercising every multi-row write.
#[derive(Clone)]
enum Op {
    Insert(ObjectRow),
    Commit(ChangesetBundle),
    Cas(RefCas),
    Redact(RedactTxn),
    Unredact(Vec<ObjectId>),
    Purge(Vec<ObjectId>),
    Derived(Vec<DerivedRow>),
    Audit(AuditRow),
}

fn workload() -> Vec<Op> {
    let mut ops = Vec::new();
    let rows: Vec<ObjectRow> = (0..6).map(blob_row).collect();
    for r in &rows {
        ops.push(Op::Insert(r.clone()));
    }
    let root = opaque_cs_row(b"root", 1);
    let child = opaque_cs_row(b"child", 2);
    let dk = |k: &[u8]| DerivedKey {
        ty: "history".to_owned(),
        version: 1,
        key: Bytes::copy_from_slice(k),
    };
    let mk_bundle = |row: scion_store::ChangesetRow, parents: Vec<ObjectId>, obj: ObjectId| {
        let id = row.id;
        ChangesetBundle {
            row,
            parents,
            object_paths: vec![ObjectPathRow {
                object: obj,
                path: RepoPath::root(),
                first_changeset: id,
                at: t(3),
            }],
            derived: vec![DerivedRow {
                key: dk(id.as_bytes()),
                value: Some(Bytes::from_static(b"node")),
                storage_key: None,
                source_object: Some(obj),
                at: t(3),
            }],
        }
    };
    ops.push(Op::Commit(mk_bundle(root.clone(), vec![], rows[0].id)));
    ops.push(Op::Commit(mk_bundle(
        child.clone(),
        vec![root.id],
        rows[1].id,
    )));
    let main = RefName::main();
    let base = RefCas {
        name: main,
        expect: RefExpect::Absent,
        new_target: Some(root.id),
        principal: "github:a".to_owned(),
        at: t(4),
        landed: vec![],
        audit: Some(audit("create_ref")),
    };
    ops.push(Op::Cas(base.clone()));
    ops.push(Op::Cas(RefCas {
        expect: RefExpect::Version(1),
        new_target: Some(child.id),
        landed: vec![
            LandedEntry {
                change_id: ChangeId([1; 16]),
                source: child.id,
                result: child.id,
            },
            LandedEntry {
                change_id: ChangeId([2; 16]),
                source: root.id,
                result: child.id,
            },
        ],
        ..base
    }));
    ops.push(Op::Redact(RedactTxn {
        ids: vec![rows[2].id, rows[3].id, rows[4].id],
        reason: "leak".to_owned(),
        actor: "github:admin".to_owned(),
        at: t(5),
        purge_after: t(50),
    }));
    ops.push(Op::Unredact(vec![rows[2].id]));
    ops.push(Op::Purge(vec![rows[3].id]));
    ops.push(Op::Derived(vec![DerivedRow {
        key: dk(b"lazy"),
        value: Some(Bytes::from_static(b"v")),
        storage_key: None,
        source_object: None,
        at: t(6),
    }]));
    ops.push(Op::Audit(audit("misc")));
    ops
}

async fn apply(m: &dyn MetaStore, op: &Op) -> Result<()> {
    match op.clone() {
        Op::Insert(r) => m.insert_object(r).await.map(|_| ()),
        Op::Commit(b) => m.commit_changeset(b).await.map(|_| ()),
        Op::Cas(c) => match m.cas_ref(c).await? {
            CasOutcome::Committed(_) => Ok(()),
            other => panic!("workload CAS failed: {other:?}"),
        },
        Op::Redact(r) => m.redact(r).await.map(|_| ()),
        Op::Unredact(ids) => m.unredact(&ids, audit("unredact")).await.map(|_| ()),
        Op::Purge(ids) => m.mark_purged(&ids, audit("purge")).await,
        Op::Derived(rows) => m.put_derived(rows).await,
        Op::Audit(a) => m.append_audit(a).await,
    }
}

/// Everything observable about a store, for whole-state comparisons.
async fn observe(
    m: &MemMetaStore,
) -> (
    TableCounts,
    Vec<ObjectRow>,
    Vec<scion_store::ChangesetRow>,
    u64,
    usize,
) {
    let refs = m.table_counts();
    (
        refs,
        m.all_objects(),
        m.all_changesets(),
        m.redaction_epoch().await.unwrap(),
        m.audit_log(1000).await.unwrap().len(),
    )
}

#[tokio::test]
async fn fail_txn_every_leaves_no_partial_state() {
    let reference = MemMetaStore::new(MemOptions::default());
    for op in workload() {
        apply(&reference, &op).await.unwrap();
    }
    let want = observe(&reference).await;

    for every in [2u32, 3, 5] {
        let m = MemMetaStore::new(MemOptions {
            fail_txn_every: Some(every),
            ..MemOptions::default()
        });
        let mut failures = 0;
        for op in workload() {
            loop {
                let before = (m.table_counts(), m.all_objects(), m.all_changesets());
                match apply(&m, &op).await {
                    Ok(()) => break,
                    Err(e) => {
                        assert_eq!(e.code, ErrorCode::Unavailable);
                        assert_eq!(e.subcode(), Some(subcode::TXN_RETRY));
                        failures += 1;
                        let after = (m.table_counts(), m.all_objects(), m.all_changesets());
                        assert_eq!(before, after, "a failed transaction changed state");
                    }
                }
            }
        }
        assert!(failures > 0, "fail_txn_every={every} never fired");
        m.set_fail_txn_every(None);
        assert_eq!(
            observe(&m).await,
            want,
            "final state with fail_txn_every={every}"
        );
    }
}

#[tokio::test]
async fn fail_txn_every_counts_reads_too() {
    let m = MemMetaStore::new(MemOptions {
        fail_txn_every: Some(2),
        ..MemOptions::default()
    });
    m.ping().await.unwrap();
    let e = m.ping().await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::TXN_RETRY));
    m.get_ref(&RefName::main()).await.unwrap();
    assert!(m.redaction_epoch().await.is_err());
}

#[tokio::test]
async fn stale_reads_lag_by_k_commits() {
    let m = MemMetaStore::new(MemOptions {
        stale_lag_commits: 2,
        ..MemOptions::default()
    });
    let rows: Vec<ObjectRow> = (0..4).map(blob_row).collect();
    let ids: Vec<ObjectId> = rows.iter().map(|r| r.id).collect();
    let visible = |v: Vec<Option<ObjectRow>>| v.iter().filter(|r| r.is_some()).count();
    m.insert_object(rows[0].clone()).await.unwrap();
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        0
    );
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Strong).await.unwrap()),
        1
    );
    m.insert_object(rows[1].clone()).await.unwrap();
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        0
    );
    m.insert_object(rows[2].clone()).await.unwrap();
    assert_eq!(
        m.get_objects(&ids, Consistency::Stale).await.unwrap(),
        vec![Some(rows[0].clone()), None, None, None],
        "stale view is 2 commits old"
    );
    // A no-op insert is not a commit.
    m.insert_object(rows[2].clone()).await.unwrap();
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        1
    );
    m.advance_stale_view();
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        3
    );
    m.insert_object(rows[3].clone()).await.unwrap();
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        3
    );
    m.set_stale_lag(0);
    assert_eq!(
        visible(m.get_objects(&ids, Consistency::Stale).await.unwrap()),
        4
    );
    // Changesets and derived rows follow the same snapshots.
    m.set_stale_lag(1);
    let cs = opaque_cs_row(b"stale cs", 1);
    m.commit_changeset(store_support::bundle(cs.clone(), vec![]))
        .await
        .unwrap();
    assert_eq!(
        m.get_changesets(&[cs.id], Consistency::Stale)
            .await
            .unwrap(),
        vec![None]
    );
    assert_eq!(
        m.get_changesets(&[cs.id], Consistency::Strong)
            .await
            .unwrap(),
        vec![Some(cs.clone())]
    );
    m.advance_stale_view();
    assert_eq!(
        m.get_changesets(&[cs.id], Consistency::Stale)
            .await
            .unwrap(),
        vec![Some(cs)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_epoch_is_atomic_with_rows_and_states() {
    let m = Arc::new(MemMetaStore::new(MemOptions::default()));
    let rows: Vec<ObjectRow> = (0..40).map(blob_row).collect();
    for r in &rows {
        m.insert_object(r.clone()).await.unwrap();
    }
    // A failing redact (unknown id) has no effect at all.
    let before = m.table_counts();
    let e = m
        .redact(RedactTxn {
            ids: vec![rows[0].id, ObjectId::hash_blob(b"nope")],
            reason: "r".to_owned(),
            actor: "a".to_owned(),
            at: t(1),
            purge_after: t(2),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert_eq!(m.table_counts(), before);
    assert_eq!(m.redaction_epoch().await.unwrap(), 0);
    assert!(m.all_objects().iter().all(|o| o.state == ObjectState::Live));

    let writers: Vec<_> = rows
        .iter()
        .map(|r| {
            let m = m.clone();
            let id = r.id;
            tokio::spawn(async move {
                m.redact(RedactTxn {
                    ids: vec![id],
                    reason: "r".to_owned(),
                    actor: "a".to_owned(),
                    at: t(1),
                    purge_after: t(2),
                })
                .await
                .unwrap()
            })
        })
        .collect();
    let reader = {
        let m = m.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                let (rows, epoch) = m.list_redactions(0).await.unwrap();
                // Every redact so far is fully visible: one row per epoch 1..=epoch.
                let epochs: Vec<u64> = rows.iter().map(|r| r.epoch).collect();
                assert_eq!(epochs, (1..=epoch).collect::<Vec<_>>());
                tokio::task::yield_now().await;
            }
        })
    };
    for w in writers {
        w.await.unwrap();
    }
    reader.await.unwrap();
    let (listed, epoch) = m.list_redactions(0).await.unwrap();
    assert_eq!(epoch, 40);
    assert_eq!(listed.len(), 40);
    assert!(
        m.all_objects()
            .iter()
            .all(|o| o.state == ObjectState::Quarantined)
    );
}

fn instrumented() -> (
    InstrumentedMeta<Arc<MemMetaStore>>,
    Arc<MemMetaStore>,
    Arc<Counters>,
    Arc<FaultRegistry>,
) {
    let raw = Arc::new(MemMetaStore::new(MemOptions::default()));
    let c = Arc::new(Counters::default());
    let f = FaultRegistry::new();
    (
        InstrumentedMeta::new(raw.clone(), c.clone(), f.clone()),
        raw,
        c,
        f,
    )
}

#[tokio::test]
async fn meta_decorator_before_after_and_fail_after() {
    let (m, raw, _c, f) = instrumented();
    let r0 = blob_row(0);
    f.arm(
        "meta.insert_object.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let e = m.insert_object(r0.clone()).await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    assert_eq!(raw.table_counts().objects, 0, "before: no effect");
    assert_eq!(f.hits("meta.insert_object.before"), 1);

    f.arm("meta.insert_object.after", FaultAction::FailAfter, Some(1));
    let e = m.insert_object(r0.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    assert_eq!(raw.table_counts().objects, 1, "FailAfter: effect applied");
    assert_eq!(
        m.insert_object(r0.clone()).await.unwrap(),
        scion_store::InsertOutcome::AlreadyExists
    );

    // An Error armed at `after` also fails after the effect.
    f.arm(
        "meta.insert_object.after",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert!(m.insert_object(blob_row(1)).await.is_err());
    assert_eq!(raw.table_counts().objects, 2);

    // Crash latches every later call until reset.
    f.arm("meta.get_ref.before", FaultAction::Crash, Some(1));
    let e = m.get_ref(&RefName::main()).await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED_CRASH));
    let e = m.insert_object(blob_row(2)).await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED_CRASH));
    assert_eq!(raw.table_counts().objects, 2);
    f.reset();
    m.insert_object(blob_row(2)).await.unwrap();
    assert_eq!(raw.table_counts().objects, 3);
}

#[tokio::test]
async fn meta_decorator_counts() {
    let (m, _raw, c, _f) = instrumented();
    let rows: Vec<ObjectRow> = (0..3).map(blob_row).collect();
    for r in &rows {
        m.insert_object(r.clone()).await.unwrap();
    }
    let s0 = c.snapshot();
    assert_eq!(s0.meta_txns, 3);
    assert_eq!(s0.meta_rows_read, 0);
    let ids = [rows[0].id, rows[1].id, ObjectId::hash_blob(b"absent")];
    m.get_objects(&ids, Consistency::Strong).await.unwrap();
    let d = c.snapshot() - s0.clone();
    assert_eq!((d.meta_txns, d.meta_rows_read), (1, 2));
    let root = opaque_cs_row(b"r", 1);
    let mut b = store_support::bundle(root.clone(), vec![]);
    for (i, r) in rows.iter().enumerate() {
        let _ = i;
        b.object_paths.push(ObjectPathRow {
            object: r.id,
            path: RepoPath::root(),
            first_changeset: root.id,
            at: t(0),
        });
    }
    m.commit_changeset(b).await.unwrap();
    let s1 = c.snapshot();
    let got = m.object_paths(&rows[1].id, 100).await.unwrap();
    assert_eq!(got.len(), 1);
    let d = c.snapshot() - s1;
    assert_eq!(
        d.object_paths_rows_read, 1,
        "only rows of the requested object"
    );
    assert_eq!(d.meta_rows_read, 1);
    assert_eq!(d.meta_txns, 1);
}

#[test]
fn leases_count_and_emit_lock_spans() {
    let cap = Capture::default();
    let (m, _raw, c, _f) = instrumented();
    with_capture(&cap, async {
        let k = DerivedKey {
            ty: "blame".to_owned(),
            version: 1,
            key: Bytes::from_static(b"k"),
        };
        assert!(
            m.try_lease(&k, "h1", Duration::from_secs(5), t(0))
                .await
                .unwrap()
        );
        assert!(
            !m.try_lease(&k, "h2", Duration::from_secs(5), t(1))
                .await
                .unwrap()
        );
        m.release_lease(&k, "h1").await.unwrap();
    });
    assert_eq!(c.snapshot().lease_calls, 3);
    let spans = cap.spans.lock().clone();
    let lock_spans = spans
        .iter()
        .filter(|s| s.starts_with(&format!("{}:", scion_core::trace::LOCK_TARGET)))
        .count();
    assert_eq!(lock_spans, 3, "spans: {spans:?}");
}

#[tokio::test]
async fn storage_in_memory_wires_decorators() {
    let (storage, handles) = Storage::in_memory(MemOptions::default());
    storage.faults.arm(
        "meta.ping.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert!(storage.meta.ping().await.is_err());
    storage.meta.ping().await.unwrap();
    assert_eq!(storage.counters.snapshot().meta_txns, 1);
    assert_eq!(storage.meta.repo_id(), handles.meta.repo_id());
    storage.faults.arm(
        "blob.get.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert!(storage.blobs.get("k").await.is_err());
    assert_eq!(storage.blobs.get("k").await.unwrap(), None);
    assert_eq!(storage.counters.snapshot().blob_gets, 1);
}

#[tokio::test]
async fn upload_batch_is_atomic_on_transaction_failure() {
    let meta = MemMetaStore::new(MemOptions::default());
    let ids = [
        ObjectId::hash_blob(b"first"),
        ObjectId::hash_blob(b"second"),
    ];
    meta.set_fail_txn_every(Some(1));
    assert_eq!(
        meta.record_uploads(&ids, "github:alice", t(1))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    meta.set_fail_txn_every(None);
    for id in &ids {
        assert!(!meta.has_upload(id, "github:alice").await.unwrap());
    }
    meta.record_uploads(&ids, "github:alice", t(2))
        .await
        .unwrap();
    for id in &ids {
        assert!(meta.has_upload(id, "github:alice").await.unwrap());
        assert!(!meta.has_upload(id, "github:bob").await.unwrap());
    }
}

#[tokio::test]
async fn batch_ownership_reads_are_strong_and_faulted_once() {
    let (storage, handles) = Storage::in_memory(MemOptions {
        stale_lag_commits: 3,
        ..MemOptions::default()
    });
    let id = ObjectId::hash_blob(b"ownership strong");
    storage
        .meta
        .record_upload(&id, "github:alice", t(1))
        .await
        .unwrap();
    let before = storage.counters.snapshot();
    assert_eq!(
        storage
            .meta
            .has_uploads(&[id, id], "github:alice")
            .await
            .unwrap(),
        vec![true, true]
    );
    assert_eq!((storage.counters.snapshot() - before).meta_txns, 1);
    storage.faults.arm(
        &scion_core::points::meta_before("has_uploads"),
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert_eq!(
        storage
            .meta
            .has_uploads(&[id], "github:alice")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        storage
            .meta
            .has_uploads(&[id], "github:alice")
            .await
            .unwrap(),
        vec![true]
    );
    handles.meta.set_fail_txn_every(Some(1));
    assert_eq!(
        storage
            .meta
            .has_uploads(&[id], "github:alice")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
}
