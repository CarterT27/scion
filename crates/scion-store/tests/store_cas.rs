//! Store-level ref CAS semantics (SPEC 4.3.6, 5.3 step 3, 8.4, 19.5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

mod store_support;

use scion_core::{ChangeId, Kind, ObjectId};
use scion_object::RefName;
use scion_store::{
    AuditRow, CasOutcome, LandedEntry, MemOptions, MetaStore, RefCas, RefExpect, Storage,
};
use std::sync::Arc;
use store_support::t;

fn cs(n: u64) -> ObjectId {
    ObjectId::hash_encoded(Kind::Changeset, &n.to_be_bytes())
}

fn cas(expect: RefExpect, target: ObjectId) -> RefCas {
    RefCas {
        name: RefName::main(),
        expect,
        new_target: Some(target),
        principal: "github:racer".to_owned(),
        at: t(1),
        landed: vec![],
        audit: None,
    }
}

fn meta() -> Arc<dyn MetaStore> {
    Storage::in_memory(MemOptions::default()).0.meta
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn thousand_concurrent_cas_one_winner() {
    let m = meta();
    let main = RefName::main();
    m.cas_ref(cas(RefExpect::Absent, cs(0))).await.unwrap();
    let handles: Vec<_> = (1..=1000u64)
        .map(|i| {
            let m = m.clone();
            tokio::spawn(async move { m.cas_ref(cas(RefExpect::Version(1), cs(i))).await })
        })
        .collect();
    let mut winners = Vec::new();
    for h in handles {
        match h.await.unwrap().unwrap() {
            CasOutcome::Committed(Some(r)) => winners.push(r),
            CasOutcome::VersionMismatch { current: Some(r) } => assert_eq!(r.version, 2),
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one Committed");
    assert_eq!(winners[0].version, 2, "version advanced by exactly 1");
    let cur = m.get_ref(&main).await.unwrap().unwrap();
    assert_eq!(cur, winners[0]);
    let log = m.ref_log(&main, 0, 10_000).await.unwrap();
    assert_eq!(
        log.iter().map(|r| r.version).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(log[1].old_target, Some(cs(0)));
    assert_eq!(log[1].new_target, Some(winners[0].target));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn contended_increments_leave_a_gapless_ref_log() {
    let m = meta();
    let main = RefName::main();
    m.cas_ref(cas(RefExpect::Absent, cs(0))).await.unwrap();
    const TASKS: u64 = 16;
    const PER_TASK: u64 = 40;
    let handles: Vec<_> = (0..TASKS)
        .map(|task| {
            let m = m.clone();
            tokio::spawn(async move {
                let main = RefName::main();
                let mut done = 0;
                let mut attempts = 0u64;
                while done < PER_TASK {
                    attempts += 1;
                    let cur = m.get_ref(&main).await.unwrap().unwrap();
                    let target = cs(1_000 + task * 1_000 + done);
                    match m
                        .cas_ref(cas(RefExpect::Version(cur.version), target))
                        .await
                        .unwrap()
                    {
                        CasOutcome::Committed(Some(r)) => {
                            assert_eq!(r.version, cur.version + 1);
                            done += 1;
                        }
                        CasOutcome::VersionMismatch { current: Some(r) } => {
                            assert!(r.version > cur.version)
                        }
                        other => panic!("unexpected {other:?}"),
                    }
                }
                attempts
            })
        })
        .collect();
    for h in handles {
        h.await.unwrap();
    }
    let final_version = 1 + TASKS * PER_TASK;
    let cur = m.get_ref(&main).await.unwrap().unwrap();
    assert_eq!(cur.version, final_version);
    let log = m.ref_log(&main, 0, 100_000).await.unwrap();
    assert_eq!(
        log.iter().map(|r| r.version).collect::<Vec<_>>(),
        (1..=final_version).collect::<Vec<_>>(),
        "ref_log has no gaps"
    );
    for w in log.windows(2) {
        assert_eq!(w[1].old_target, w[0].new_target, "ref_log chains targets");
    }
    assert_eq!(log.last().unwrap().new_target, Some(cur.target));
}

#[tokio::test]
async fn aba_cas_with_first_version_fails() {
    let m = meta();
    let main = RefName::main();
    let (a, b) = (cs(1), cs(2));
    m.cas_ref(cas(RefExpect::Absent, a)).await.unwrap();
    m.cas_ref(cas(RefExpect::Version(1), b)).await.unwrap();
    m.cas_ref(cas(RefExpect::Version(2), a)).await.unwrap();
    let cur = m.get_ref(&main).await.unwrap().unwrap();
    assert_eq!((cur.target, cur.version), (a, 3), "target is back to A");
    let o = m.cas_ref(cas(RefExpect::Version(1), b)).await.unwrap();
    assert_eq!(o, CasOutcome::VersionMismatch { current: Some(cur) });
    assert_eq!(m.ref_log(&main, 0, 100).await.unwrap().len(), 3);
}

#[tokio::test]
async fn landed_conflict_rolls_back_the_whole_cas() {
    let (storage, handles) = Storage::in_memory(MemOptions::default());
    let m = storage.meta;
    let main = RefName::main();
    let ch = ChangeId([5; 16]);
    m.cas_ref(cas(RefExpect::Absent, cs(1))).await.unwrap();
    let mut first = cas(RefExpect::Version(1), cs(2));
    first.landed = vec![LandedEntry {
        change_id: ch,
        source: cs(20),
        result: cs(2),
    }];
    m.cas_ref(first).await.unwrap();
    let before = handles.meta.table_counts();
    let audit_before = m.audit_log(100).await.unwrap();

    let mut second = cas(RefExpect::Version(2), cs(3));
    second.landed = vec![
        LandedEntry {
            change_id: ChangeId([6; 16]),
            source: cs(30),
            result: cs(3),
        },
        LandedEntry {
            change_id: ch,
            source: cs(31),
            result: cs(3),
        },
    ];
    second.audit = Some(AuditRow {
        at: t(2),
        actor: "github:admin".to_owned(),
        action: "update_ref".to_owned(),
        target: "main".to_owned(),
        reason: "r".to_owned(),
        detail: String::new(),
    });
    match m.cas_ref(second).await.unwrap() {
        CasOutcome::LandedConflict { existing } => {
            assert_eq!(existing.change_id, ch);
            assert_eq!(existing.version, 2);
            assert_eq!(existing.source, cs(20));
            assert_eq!(existing.result, cs(2));
        }
        other => panic!("expected LandedConflict, got {other:?}"),
    }
    assert_eq!(handles.meta.table_counts(), before, "no row changed");
    assert_eq!(
        m.audit_log(100).await.unwrap(),
        audit_before,
        "no audit row"
    );
    let cur = m.get_ref(&main).await.unwrap().unwrap();
    assert_eq!((cur.version, cur.target), (2, cs(2)));
    assert!(
        m.get_landed(&main, &ChangeId([6; 16]))
            .await
            .unwrap()
            .is_none()
    );
}
