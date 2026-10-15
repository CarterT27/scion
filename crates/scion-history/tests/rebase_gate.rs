//! SPEC 19.18 unit gate: on shared-subtree repositories of 10^3 and 10^5 files (10^7 in the
//! ignored `full_` variant) with equal depth, `Rebase` of a 1-file change over an unrelated tip
//! reads zero blobs and O(|delta| x depth) trees, independent of repository size.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use bytes::Bytes;
use merge_support::*;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_history::HistoryConfig;
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, Meta, Timestamp};
use scion_store::{ChangesetBundle, ChangesetRow, CounterSnapshot};
use scion_testkit::shared_subtree_repo;

/// Depth (path components of every file) used for every size.
const DEPTH: u32 = 5;

async fn put_file(env: &Env, path: scion_object::RepoPath, content: &[u8]) -> DeltaEntry {
    let e = upload(&env.h, Mode::File, content).await;
    DeltaEntry {
        path,
        op: DeltaOp::Put {
            mode: e.mode,
            id: e.id,
            size: e.size,
        },
    }
}

/// Load the repository as a parentless fixture changeset `B0`, create `C = B0 + perturbed
/// file` and `X = B0 + unrelated root file`, then rebase `C` onto `X` and return the counter
/// delta of the rebase alone.
async fn rebase_counters(files: u64) -> (u32, CounterSnapshot) {
    let repo = shared_subtree_repo(files, DEPTH);
    let env = env_with(HistoryConfig::default()).await;
    let h = &env.h;
    for (id, kind, bytes) in repo.encoded_objects() {
        let stored = match kind {
            Kind::Blob => h.objects().put_blob(bytes).await.unwrap().0,
            k => h.objects().put_encoded(k, bytes).await.unwrap().0,
        };
        assert_eq!(stored, id);
    }
    let meta = Meta {
        author: b"fixture".to_vec(),
        author_time: Timestamp {
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
                fp_merges: None,
                change_id: base.change_id,
                created_by: "service:fixture".to_owned(),
                created_at: std::time::SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths: Vec::new(),
            derived: Vec::new(),
        })
        .await
        .unwrap();

    let change = put_file(&env, repo.perturb_path(), b"perturbed content\n").await;
    let c = commit_delta(h, vec![base_id], vec![change], "change").await;
    let unrelated = put_file(&env, p("zz-unrelated"), b"unrelated\n").await;
    let x = commit_delta(h, vec![base_id], vec![unrelated], "unrelated").await;

    let counters = env.storage.counters.clone();
    let before = counters.snapshot();
    let r = h.rebase(&c, &x, &alice()).await.unwrap();
    let diff = counters.snapshot() - before;
    let rec = h.changeset(&r).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![x]);
    assert_eq!(rec.changeset.delta.len(), 1);
    (repo.depth, diff)
}

async fn gate(files: u64) -> CounterSnapshot {
    let (depth, d) = rebase_counters(files).await;
    let depth = u64::from(depth);
    eprintln!(
        "19.18 rebase gate: {files} files, depth {depth}: trees_read {}, blobs_read {}, \
         trees_written {}, meta_txns {}",
        d.trees_read, d.blobs_read, d.trees_written, d.meta_txns
    );
    assert_eq!(
        d.blobs_read, 0,
        "{files} files: rebase over an unrelated tip"
    );
    // |delta| = 1: the walks of B and O plus apply_delta over O.
    assert!(
        d.trees_read <= 3 * depth + 2,
        "{files} files: read {} trees (depth {depth})",
        d.trees_read
    );
    d
}

#[tokio::test]
async fn gate_1e3_and_1e5_are_size_independent() {
    let small = gate(1_000).await;
    let large = gate(100_000).await;
    assert_eq!(small.trees_read, large.trees_read);
    assert_eq!(small.blobs_read, large.blobs_read);
    assert_eq!(small.trees_written, large.trees_written);
    assert_eq!(small.meta_txns, large.meta_txns);
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): 10^7 files"]
async fn full_gate_1e7() {
    let small = gate(1_000).await;
    let huge = gate(10_000_000).await;
    assert_eq!(small.trees_read, huge.trees_read);
    assert_eq!(small.blobs_read, huge.blobs_read);
    assert_eq!(small.meta_txns, huge.meta_txns);
}
