//! SPEC 19.18: `Land` of a 1-file change performs a constant number of metadata transactions
//! per attempt, equal on shared-subtree repositories of 10^3 and 10^5 files with equal depth
//! (10^7 in the ignored `full_` variant). The fake rebaser re-applies the delta onto a moved tip
//! via `create_internal`, so every attempt does a real rebase-shaped write.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod land_support;

use bytes::Bytes;
use land_support::*;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_history::{AllowAll, History, HistoryConfig, LandAttemptOutcome, LandRequest, NoEager};
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, RepoPath};
use scion_store::{
    ChangesetBundle, ChangesetRow, CounterSnapshot, MemOptions, ObjectRepo, ObjectRepoConfig,
    Storage,
};
use scion_testkit::shared_subtree_repo;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const DEPTH: u32 = 5;

/// Per-size measurements: (single-attempt land, contended land with 2 lost races).
struct Gate {
    single: CounterSnapshot,
    contended: CounterSnapshot,
    contended_attempts: usize,
}

async fn one_file_put(h: &History, path: RepoPath, content: &str) -> DeltaEntry {
    let (blob, _) = h
        .objects()
        .put_blob(Bytes::from(content.as_bytes().to_vec()))
        .await
        .unwrap();
    DeltaEntry {
        path,
        op: DeltaOp::Put {
            mode: Mode::File,
            id: blob,
            size: content.len() as u64,
        },
    }
}

async fn measure(files: u64) -> Gate {
    let repo = shared_subtree_repo(files, DEPTH);
    let mem = Arc::new(scion_store::MemMetaStore::new(MemOptions::default()));
    let contended = Arc::new(ContendedMeta {
        inner: mem.clone(),
        forced_mismatches: Default::default(),
        cas_calls: Default::default(),
    });
    let storage = Storage::new(
        contended.clone(),
        Arc::new(scion_store::MemBlobStore::new()),
        scion_core::FaultRegistry::new(),
    );
    let counters = storage.counters.clone();
    let clock = scion_core::ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    let observer = Arc::new(RecordingObserver::default());
    let h = Arc::new(
        History::new(objects, HistoryConfig::default(), Arc::new(NoEager), clock)
            .with_land_observer(observer.clone()),
    );
    for (id, kind, bytes) in repo.encoded_objects() {
        let stored = match kind {
            Kind::Blob => h.objects().put_blob(bytes).await.unwrap().0,
            k => h.objects().put_encoded(k, bytes).await.unwrap().0,
        };
        assert_eq!(stored, id);
    }
    let meta = scion_object::Meta {
        author: b"fixture".to_vec(),
        author_time: scion_object::Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"service:fixture".to_vec(),
        message: b"fixture".to_vec(),
        extensions: Extensions::new(),
    };
    let (meta_id, _) = h
        .objects()
        .put_encoded(Kind::Meta, Bytes::from(meta.encode()))
        .await
        .unwrap();
    let base = Changeset {
        parents: Vec::new(),
        root_tree: repo.root_tree(),
        meta: meta_id,
        change_id: ChangeId([7; 16]),
        delta: Vec::new(),
        extensions: Extensions::new(),
    };
    let encoded = Bytes::from(base.encode());
    let base_id = ObjectId::hash_encoded(Kind::Changeset, &encoded);
    h.meta()
        .commit_changeset(ChangesetBundle {
            row: ChangesetRow {
                id: base_id,
                encoded,
                generation: 1,
                skip: Vec::new(),
                change_id: base.change_id,
                created_by: "service:fixture".to_owned(),
                created_at: SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths: Vec::new(),
            derived: Vec::new(),
        })
        .await
        .unwrap();
    h.create_ref(&main(), &base_id, &alice(), "fixture")
        .await
        .unwrap();

    // Move main with an unrelated 1-file change at the same depth, so the measured lands rebase.
    let perturb = repo.perturb_path();
    let mut other = perturb.as_bytes().to_vec();
    other.extend_from_slice(b".other");
    let other = RepoPath::parse(&other).unwrap();
    let mover_delta = vec![one_file_put(&h, other, "other\n").await];
    let mover = commit_delta(&h, vec![base_id], mover_delta, ChangeId([8; 16])).await;
    let rebaser = FakeRebaser { h: h.clone() };
    let land = |head| {
        let h = h.clone();
        let rebaser = &rebaser;
        async move {
            h.land_with(
                LandRequest {
                    ref_name: main(),
                    head,
                    allow_empty: false,
                },
                &alice(),
                &AllowAll,
                rebaser,
            )
            .await
            .unwrap()
        }
    };
    land(mover).await;
    observer.take();

    // Single-attempt 1-file land.
    let d1 = vec![one_file_put(&h, perturb.clone(), "perturbed 1\n").await];
    let c1 = commit_delta(&h, vec![base_id], d1, ChangeId([9; 16])).await;
    let before = counters.snapshot();
    let r1 = land(c1).await;
    let single = counters.snapshot() - before;
    assert_eq!(observer.take(), vec![(1, LandAttemptOutcome::Committed)]);
    assert_ne!(r1.target, c1);

    // Contended 1-file land: two lost CAS races, then the commit.
    let sibling = perturb
        .parent()
        .unwrap()
        .join(&scion_object::Name::new(b"contended.txt").unwrap())
        .unwrap();
    let d2 = vec![one_file_put(&h, sibling, "perturbed 2\n").await];
    let c2 = commit_delta(&h, vec![base_id], d2, ChangeId([10; 16])).await;
    contended.force_mismatches(2);
    let before = counters.snapshot();
    land(c2).await;
    let contended_delta = counters.snapshot() - before;
    let attempts = observer.take();
    assert_eq!(
        attempts.last().unwrap(),
        &(3, LandAttemptOutcome::Committed)
    );

    eprintln!(
        "19.18 land gate: {files} files, depth {}: single-attempt meta_txns {}, \
         3-attempt meta_txns {}, trees_read {}",
        repo.depth, single.meta_txns, contended_delta.meta_txns, single.trees_read
    );
    Gate {
        single,
        contended: contended_delta,
        contended_attempts: attempts.len(),
    }
}

fn check(small: &Gate, large: &Gate) {
    assert_eq!(
        small.single.meta_txns, large.single.meta_txns,
        "1-file land: metadata transactions independent of repository size"
    );
    assert_eq!(small.contended.meta_txns, large.contended.meta_txns);
    assert_eq!(small.contended_attempts, large.contended_attempts);
    assert_eq!(small.single.lease_calls + large.contended.lease_calls, 0);
    // A small constant: ref read, landed read, the rebase write path, one CAS.
    assert!(
        small.single.meta_txns <= 32,
        "single attempt used {} meta txns",
        small.single.meta_txns
    );
    // Every retry adds at most the cost of one attempt.
    assert!(
        small.contended.meta_txns <= 3 * small.single.meta_txns,
        "3 attempts used {} meta txns (1 attempt: {})",
        small.contended.meta_txns,
        small.single.meta_txns
    );
}

#[tokio::test]
async fn land_meta_txns_per_attempt_are_size_independent() {
    let small = measure(1_000).await;
    let large = measure(100_000).await;
    check(&small, &large);
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): 10^7 files"]
async fn full_land_gate_1e7() {
    let small = measure(1_000).await;
    let huge = measure(10_000_000).await;
    check(&small, &huge);
}
