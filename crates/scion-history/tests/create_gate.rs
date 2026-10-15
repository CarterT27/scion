//! SPEC 19.18 unit gate: on shared-subtree repositories of 10^3 and 10^5 files (10^7 in the
//! ignored `full_` variant) with equal depth, a 1-file CreateChangeset reads at most
//! `depth + 2` trees and writes at most `depth + 1` trees, independent of repository size.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod create_support;

use bytes::Bytes;
use create_support::*;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_history::{AllowAll, HistoryConfig};
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions};
use scion_store::{ChangesetBundle, ChangesetRow, CounterSnapshot};
use scion_testkit::shared_subtree_repo;

/// Depth (path components of every file) used for every size.
const DEPTH: u32 = 5;

/// Load the shared-subtree repository and commit a parentless fixture changeset whose root is
/// that tree (the delta is irrelevant for the gate and left empty), then run one 1-file
/// CreateChangeset and return the counter delta.
async fn one_file_change(files: u64) -> (u32, CounterSnapshot) {
    let repo = shared_subtree_repo(files, DEPTH);
    let (h, _mem, _clock) = history_with(HistoryConfig::default());
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

    let content = b"perturbed content\n".to_vec();
    let (blob, _) = h
        .objects()
        .put_blob(Bytes::from(content.clone()))
        .await
        .unwrap();
    let delta = vec![DeltaEntry {
        path: repo.perturb_path(),
        op: DeltaOp::Put {
            mode: Mode::File,
            id: blob,
            size: content.len() as u64,
        },
    }];
    let counters = h.objects().storage().counters.clone();
    let before = counters.snapshot();
    let created = h
        .create_changeset(req(vec![base_id], delta), &alice(), &AllowAll)
        .await
        .unwrap();
    let diff = counters.snapshot() - before;
    assert_ne!(created.root_tree, repo.root_tree());
    (repo.depth, diff)
}

async fn gate(files: u64) -> CounterSnapshot {
    let (depth, d) = one_file_change(files).await;
    let depth = u64::from(depth);
    eprintln!(
        "19.18 gate: {files} files, depth {depth}: trees_read {}, trees_written {}",
        d.trees_read, d.trees_written
    );
    assert!(
        d.trees_read <= depth + 2,
        "{files} files: read {} trees (depth {depth})",
        d.trees_read
    );
    assert!(
        d.trees_written <= depth + 1,
        "{files} files: wrote {} trees (depth {depth})",
        d.trees_written
    );
    assert_eq!(
        d.blobs_read, 0,
        "{files} files: CreateChangeset reads no blob content"
    );
    d
}

#[tokio::test]
async fn gate_1e3_and_1e5_are_size_independent() {
    let small = gate(1_000).await;
    let large = gate(100_000).await;
    assert_eq!(small.trees_read, large.trees_read);
    assert_eq!(small.trees_written, large.trees_written);
    assert_eq!(small.meta_txns, large.meta_txns);
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): 10^7 files"]
async fn full_gate_1e7() {
    let small = gate(1_000).await;
    let huge = gate(10_000_000).await;
    assert_eq!(small.trees_read, huge.trees_read);
    assert_eq!(small.trees_written, huge.trees_written);
}
