//! ObjectRepo: SPEC 19.3 storage layer, 19.1 blob ids vs compression, 16.1 class 7 integrity,
//! 19.0 counters, 13.2 byte moves and re-introduction blocking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

mod store_support;

use bytes::Bytes;
use scion_core::{
    ErrorCode, ErrorPayload, FaultAction, Kind, ObjectId, empty_tree_id, points, subcode,
};
use scion_object::{Canonical, Extensions, Name, Tree, TreeEntry};
use scion_store::{
    Area, AuditRow, CounterSnapshot, InsertOutcome, MemHandles, MemOptions, MetaStore, ObjectRepo,
    ObjectRepoConfig, ObjectRow, ObjectState, RedactTxn, StoredFrame,
};
use store_support::{
    Capture, EMPTY_TREE, assert_code, bundle, inline_row, opaque_cs_row, repo, repo_with,
    skip_without_codec, t, test_meta, with_capture,
};

fn cfg(threshold: usize) -> ObjectRepoConfig {
    ObjectRepoConfig {
        inline_threshold_bytes: threshold,
        ..ObjectRepoConfig::default()
    }
}

/// Every live, non-inline objects row has durable bytes at its live key.
fn assert_rows_imply_bytes(repo: &ObjectRepo, h: &MemHandles) {
    for row in h.meta.all_objects() {
        if row.state == ObjectState::Live && row.inline_data.is_none() {
            assert!(
                h.blobs.raw(&repo.key(&row.id, Area::Live)).is_some(),
                "objects row {} without bytes",
                row.id
            );
        }
    }
}

#[tokio::test]
async fn blob_fault_before_put_leaves_no_row() {
    let (repo, h) = repo();
    repo.storage().faults.arm(
        "blob.put.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let e = repo
        .put_blob(Bytes::from_static(b"hello"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(h.blobs.keys().is_empty());
    assert_eq!(h.meta.table_counts().objects, 0);
    let id = ObjectId::hash_blob(b"hello");
    assert_eq!(repo.stat(&id).await.unwrap(), None);
}

#[tokio::test]
async fn fail_after_on_blob_put_leaves_an_invisible_orphan() {
    let (repo, h) = repo();
    repo.storage()
        .faults
        .arm("blob.put.after", FaultAction::FailAfter, Some(1));
    let id = ObjectId::hash_blob(b"orphan");
    let e = repo
        .put_blob(Bytes::from_static(b"orphan"))
        .await
        .unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    assert_eq!(
        h.blobs.keys(),
        vec![repo.key(&id, Area::Live)],
        "bytes landed"
    );
    assert_eq!(h.meta.table_counts().objects, 0, "row insert never ran");
    assert_eq!(repo.stat(&id).await.unwrap(), None);
    assert_eq!(repo.stat_many(&[id]).await.unwrap(), vec![None]);
    assert_code(repo.get(&id).await, ErrorCode::NotFound);
    // A retry completes the upload.
    let (id2, o) = repo.put_blob(Bytes::from_static(b"orphan")).await.unwrap();
    assert_eq!((id2, o), (id, InsertOutcome::Inserted));
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::from_static(b"orphan"));
}

#[tokio::test]
async fn crash_between_bytes_and_row() {
    let (repo, h) = repo();
    let f = &repo.storage().faults;
    f.arm(points::UPLOAD_AFTER_BYTES, FaultAction::Crash, Some(1));
    let id = ObjectId::hash_blob(b"crash");
    let e = repo
        .put_blob(Bytes::from_static(b"crash"))
        .await
        .unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED_CRASH));
    assert_eq!(h.meta.table_counts().objects, 0);
    assert_eq!(h.blobs.keys().len(), 1);
    f.reset();
    assert_eq!(repo.stat(&id).await.unwrap(), None);
    repo.put_blob(Bytes::from_static(b"crash")).await.unwrap();
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::from_static(b"crash"));
}

#[tokio::test]
async fn fail_after_on_row_insert_keeps_row_and_bytes() {
    let (repo, h) = repo();
    repo.storage()
        .faults
        .arm("meta.insert_object.after", FaultAction::FailAfter, Some(1));
    let id = ObjectId::hash_blob(b"committed");
    assert!(
        repo.put_blob(Bytes::from_static(b"committed"))
            .await
            .is_err()
    );
    assert_eq!(h.meta.table_counts().objects, 1);
    assert_rows_imply_bytes(&repo, &h);
    let (_, o) = repo
        .put_blob(Bytes::from_static(b"committed"))
        .await
        .unwrap();
    assert_eq!(o, InsertOutcome::AlreadyExists);
    assert_eq!(
        repo.get(&id).await.unwrap(),
        Bytes::from_static(b"committed")
    );
}

#[tokio::test]
async fn a_row_never_exists_without_bytes() {
    let points_to_hit = [
        "blob.put.before",
        "blob.put.after",
        "meta.get_objects.before",
        "meta.get_objects.after",
        "meta.insert_object.before",
        "meta.insert_object.after",
        points::UPLOAD_AFTER_BYTES,
        points::UPLOAD_AFTER_ROW,
    ];
    let actions = [
        FaultAction::Error(ErrorCode::Unavailable),
        FaultAction::Crash,
        FaultAction::FailAfter,
    ];
    let (repo, h) = repo_with(MemOptions::default(), cfg(16));
    let mut n = 0u32;
    for p in points_to_hit {
        for a in &actions {
            n += 1;
            let f = &repo.storage().faults;
            f.reset();
            f.arm(p, a.clone(), Some(1));
            let blob = Bytes::from(format!("blob number {n}"));
            let _ = repo.put_blob(blob.clone()).await;
            // A large derived object (object storage) and a small one (inline).
            let big = Bytes::from(vec![n as u8; 100]);
            let _ = repo.put_encoded(Kind::Derived(0x11), big).await;
            assert_rows_imply_bytes(&repo, &h);
            f.reset();
            let (id, _) = repo.put_blob(blob.clone()).await.unwrap();
            assert_eq!(repo.get(&id).await.unwrap(), blob, "retry after {p}");
        }
    }
    assert_rows_imply_bytes(&repo, &h);
}

#[tokio::test]
async fn inline_threshold_boundaries_for_opaque_derived_objects() {
    const T: usize = 64;
    let (repo, h) = repo_with(MemOptions::default(), cfg(T));
    for (len, inline) in [(T - 1, true), (T, true), (T + 1, false)] {
        let data = Bytes::from(vec![0xa5u8; len]);
        let (id, o) = repo
            .put_encoded(Kind::Derived(0x11), data.clone())
            .await
            .unwrap();
        assert_eq!(o, InsertOutcome::Inserted);
        let row = repo.stat(&id).await.unwrap().unwrap();
        let key = repo.key(&id, Area::Live);
        assert_eq!(row.size, len as u64);
        if inline {
            assert_eq!(row.inline_data.as_ref(), Some(&data), "{len} bytes inline");
            assert_eq!(row.stored_size, len as u64);
            assert!(
                h.blobs.raw(&key).is_none(),
                "{len} bytes not in object storage"
            );
            assert!(
                matches!(repo.get_frame(&id).await.unwrap().0, StoredFrame::Raw(b) if b == data)
            );
        } else {
            assert_eq!(row.inline_data, None, "{len} bytes in object storage");
            let frame = h.blobs.raw(&key).expect("frame stored");
            assert_eq!(row.stored_size, frame.len() as u64);
            assert_eq!(zstd::decode_all(&frame[..]).unwrap(), data.to_vec());
            assert!(
                matches!(repo.get_frame(&id).await.unwrap().0, StoredFrame::Zstd(b) if b == frame)
            );
        }
        assert_eq!(repo.get(&id).await.unwrap(), data, "{len} bytes read back");
    }
    // Blobs always go to object storage, even when tiny.
    let (id, _) = repo.put_blob(Bytes::from_static(b"x")).await.unwrap();
    let row = repo.stat(&id).await.unwrap().unwrap();
    assert_eq!(row.inline_data, None);
    assert!(h.blobs.raw(&repo.key(&id, Area::Live)).is_some());
}

/// A tree whose canonical encoding is exactly `len` bytes (one entry, padded name).
fn tree_of_len(len: usize) -> Vec<u8> {
    let make = |name_len: usize| {
        Tree {
            entries: vec![TreeEntry {
                name: Name::new(&vec![b'n'; name_len]).unwrap(),
                mode: scion_core::Mode::File,
                id: ObjectId::hash_blob(b"x"),
                size: 1,
            }],
            extensions: Extensions::new(),
        }
        .encode()
    };
    let base = make(1).len();
    let enc = make(1 + len - base);
    assert_eq!(enc.len(), len);
    enc
}

#[tokio::test]
async fn inline_threshold_boundaries_for_trees() {
    if skip_without_codec("inline_threshold_boundaries_for_trees") {
        return;
    }
    const T: usize = 100;
    let (repo, h) = repo_with(MemOptions::default(), cfg(T));
    for (len, inline) in [(T - 1, true), (T, true), (T + 1, false)] {
        let enc = Bytes::from(tree_of_len(len));
        let (id, _) = repo.put_encoded(Kind::Tree, enc.clone()).await.unwrap();
        let row = repo.stat(&id).await.unwrap().unwrap();
        assert_eq!(row.inline_data.is_some(), inline, "tree of {len} bytes");
        assert_eq!(
            h.blobs.raw(&repo.key(&id, Area::Live)).is_some(),
            !inline,
            "tree of {len} bytes"
        );
        assert_eq!(repo.get(&id).await.unwrap(), enc);
        assert_eq!(repo.get_tree(&id).await.unwrap().encode(), enc.to_vec());
    }
    // Metas follow the same rule.
    let meta = Bytes::from(test_meta("hello").encode());
    let (mid, _) = repo.put_encoded(Kind::Meta, meta.clone()).await.unwrap();
    assert!(
        repo.stat(&mid)
            .await
            .unwrap()
            .unwrap()
            .inline_data
            .is_some()
    );
    assert_eq!(
        repo.get_meta(&mid).await.unwrap().message,
        b"hello".to_vec()
    );
}

#[tokio::test]
async fn invalid_encodings_are_rejected_before_persisting() {
    if skip_without_codec("invalid_encodings_are_rejected_before_persisting") {
        return;
    }
    let (repo, h) = repo();
    for kind in [Kind::Tree, Kind::Meta, Kind::Derived(0x10)] {
        let e = repo
            .put_encoded(kind, Bytes::from_static(b"\x01\xff"))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{kind:?}");
    }
    // A non-minimal varint (format_version 1 encoded in two bytes).
    let e = repo
        .put_encoded(Kind::Tree, Bytes::from_static(b"\x81\x00\x00\x00"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(h.meta.table_counts().objects, 0);
    assert!(h.blobs.keys().is_empty());
}

#[tokio::test]
async fn kind_and_size_checks() {
    let (repo, _h) = repo_with(
        MemOptions::default(),
        ObjectRepoConfig {
            max_blob_bytes: 10,
            ..ObjectRepoConfig::default()
        },
    );
    let e = assert_code(
        repo.put_blob(Bytes::from_static(b"01234567890")).await,
        ErrorCode::TooLarge,
    );
    assert_eq!(
        e.payload,
        ErrorPayload::Limit {
            name: "max_blob_bytes".to_owned(),
            limit: 10,
            actual: 11
        }
    );
    repo.put_blob(Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    let big = Bytes::from_static(b"01234567890");
    assert_code(
        repo.put_verified(&ObjectId::hash_blob(&big), big.clone())
            .await,
        ErrorCode::TooLarge,
    );
    let e = assert_code(
        repo.put_encoded(Kind::Changeset, Bytes::from_static(b"x"))
            .await,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(e.subcode(), Some(subcode::INVALID_KIND));
    let e = assert_code(
        repo.put_encoded(Kind::Blob, Bytes::from_static(b"x")).await,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(e.subcode(), Some(subcode::INVALID_KIND));
}

#[tokio::test]
async fn put_verified_checks_the_claimed_id() {
    let (repo, _h) = repo();
    let data = Bytes::from_static(b"claimed");
    let wrong = ObjectId::hash_blob(b"something else");
    let before = repo.storage().counters.snapshot();
    assert_code(
        repo.put_verified(&wrong, data.clone()).await,
        ErrorCode::Integrity,
    );
    assert_eq!(
        repo.storage().counters.snapshot().integrity_failures,
        before.integrity_failures,
        "a bad upload is not a store integrity failure"
    );
    let id = ObjectId::hash_blob(&data);
    assert_eq!(
        repo.put_verified(&id, data.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(
        repo.put_verified(&id, data.clone()).await.unwrap(),
        InsertOutcome::AlreadyExists
    );
    // Same digest, different kind: distinct objects, never confused (SPEC 4.2, 19.1).
    let as_derived = ObjectId::new(Kind::Derived(0x11), *id.digest());
    assert_eq!(
        as_derived,
        ObjectId::hash_encoded(Kind::Derived(0x11), &data)
    );
    assert_code(repo.get(&as_derived).await, ErrorCode::NotFound);
    assert_eq!(repo.stat(&as_derived).await.unwrap(), None);
    assert_eq!(
        repo.put_verified(&as_derived, data.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(repo.get(&as_derived).await.unwrap(), data);
    assert_eq!(
        repo.stat(&as_derived).await.unwrap().unwrap().kind,
        Kind::Derived(0x11)
    );
    assert_eq!(repo.stat(&id).await.unwrap().unwrap().kind, Kind::Blob);
}

#[tokio::test]
async fn stale_miss_is_retried_strongly() {
    let (repo, h) = repo_with(
        MemOptions {
            stale_lag_commits: 5,
            ..MemOptions::default()
        },
        ObjectRepoConfig::default(),
    );
    let c = &repo.storage().counters;
    let (id, _) = repo.put_blob(Bytes::from_static(b"fresh")).await.unwrap();
    assert!(
        h.meta
            .get_objects(&[id], scion_store::Consistency::Stale)
            .await
            .unwrap()[0]
            .is_none(),
        "the stale view misses the new row"
    );
    let s0 = c.snapshot();
    let row = repo
        .stat(&id)
        .await
        .unwrap()
        .expect("strong retry finds it");
    assert_eq!(row.id, id);
    assert_eq!(
        (c.snapshot() - s0).meta_txns,
        2,
        "stale read, then strong read"
    );
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::from_static(b"fresh"));
    assert_eq!(repo.stat_many(&[id]).await.unwrap()[0].as_ref(), Some(&row));
    // Changesets too.
    let cs = opaque_cs_row(b"fresh changeset", 1);
    repo.storage()
        .meta
        .commit_changeset(bundle(cs.clone(), vec![]))
        .await
        .unwrap();
    let st = repo.stat(&cs.id).await.unwrap().unwrap();
    assert_eq!(
        (st.kind, st.size),
        (Kind::Changeset, cs.encoded.len() as u64)
    );
    assert_eq!(repo.get(&cs.id).await.unwrap(), cs.encoded);
    // A real miss costs both reads before NOT_FOUND.
    let missing = ObjectId::hash_blob(b"missing");
    let s1 = c.snapshot();
    assert_code(repo.get(&missing).await, ErrorCode::NotFound);
    assert_eq!((c.snapshot() - s1).meta_txns, 2);
    // Once the stale view catches up, one read suffices.
    h.meta.advance_stale_view();
    let s2 = c.snapshot();
    repo.stat(&id).await.unwrap().unwrap();
    assert_eq!((c.snapshot() - s2).meta_txns, 1);
}

#[tokio::test]
async fn orphan_bytes_do_not_affect_results() {
    let (repo, h) = repo();
    let content = b"orphaned content";
    let id = ObjectId::hash_blob(content);
    let frame = zstd::encode_all(&content[..], 3).unwrap();
    h.blobs
        .insert_raw(&repo.key(&id, Area::Live), Bytes::from(frame));
    assert_eq!(repo.stat(&id).await.unwrap(), None);
    assert_eq!(repo.stat_many(&[id, id]).await.unwrap(), vec![None, None]);
    assert_code(repo.get(&id).await, ErrorCode::NotFound);
    assert_code(repo.get_frame(&id).await, ErrorCode::NotFound);
    let (id2, o) = repo.put_blob(Bytes::from_static(content)).await.unwrap();
    assert_eq!((id2, o), (id, InsertOutcome::Inserted));
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::from_static(content));
}

#[tokio::test]
async fn blob_id_is_independent_of_zstd_level_and_framing() {
    let content: Vec<u8> = (0..50_000u32)
        .flat_map(|i| (i % 97).to_le_bytes())
        .collect();
    let want = ObjectId::hash_blob(&content);
    let mut frames = Vec::new();
    for level in [1, 3, 19] {
        let (repo, h) = repo_with(
            MemOptions::default(),
            ObjectRepoConfig {
                zstd_level: level,
                ..ObjectRepoConfig::default()
            },
        );
        let (id, _) = repo.put_blob(Bytes::from(content.clone())).await.unwrap();
        assert_eq!(id, want, "level {level}");
        assert_eq!(repo.get(&id).await.unwrap(), content);
        frames.push(h.blobs.raw(&repo.key(&id, Area::Live)).unwrap());
    }
    assert_ne!(frames[0], frames[2], "levels produce different frames");
    // A multi-frame encoding of the same content is accepted and verifies.
    let (repo, h) = repo();
    let (a, b) = content.split_at(12_345);
    let mut multi = zstd::encode_all(a, 1).unwrap();
    multi.extend(zstd::encode_all(b, 19).unwrap());
    h.blobs
        .insert_raw(&repo.key(&want, Area::Live), Bytes::from(multi.clone()));
    h.meta.put_object_row_raw(ObjectRow {
        id: want,
        kind: Kind::Blob,
        size: content.len() as u64,
        stored_size: multi.len() as u64,
        inline_data: None,
        state: ObjectState::Live,
        created_at: t(0),
    });
    assert_eq!(repo.get(&want).await.unwrap(), content);
    let (frame, _) = repo.get_frame(&want).await.unwrap();
    assert!(
        matches!(frame, StoredFrame::Zstd(f) if f == multi),
        "frame served as stored"
    );
}

#[test]
fn corrupt_bytes_give_integrity_and_alert() {
    let cap = Capture::default();
    let (repo, h) = repo();
    with_capture(&cap, async {
        let (id, _) = repo
            .put_blob(Bytes::from_static(b"precious data"))
            .await
            .unwrap();
        let before = repo.storage().counters.snapshot();
        h.blobs.corrupt(&repo.key(&id, Area::Live));
        let e = repo.get(&id).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Integrity);
        assert!(e.retryable());
        assert_code(repo.get_frame(&id).await, ErrorCode::Integrity);
        let d = repo.storage().counters.snapshot() - before;
        assert_eq!(d.integrity_failures, 2);
        assert_eq!(d.blobs_read, 0, "failed reads are not counted as reads");

        // Corrupt inline data is detected as well.
        let mut row = inline_row(Kind::Tree, EMPTY_TREE);
        h.meta.put_object_row_raw(row.clone());
        row.inline_data = Some(Bytes::from_static(b"\x01\x00\x01"));
        h.meta.put_object_row_raw(row.clone());
        assert_code(repo.get(&row.id).await, ErrorCode::Integrity);

        // Bytes lost from under a live row are an integrity failure too.
        let (id2, _) = repo
            .put_blob(Bytes::from_static(b"vanishing"))
            .await
            .unwrap();
        h.blobs.remove_raw(&repo.key(&id2, Area::Live));
        assert_code(repo.get(&id2).await, ErrorCode::Integrity);
        assert_eq!(repo.storage().counters.snapshot().integrity_failures, 4);
    });
    let events = cap.events.lock().clone();
    let alerts: Vec<&String> = events
        .iter()
        .filter(|e| e.starts_with("ERROR") && e.contains("event=integrity_alert"))
        .collect();
    assert_eq!(alerts.len(), 4, "events: {events:?}");
    assert!(
        alerts
            .iter()
            .all(|a| a.contains("object=b1") || a.contains("object=t1"))
    );
}

#[tokio::test]
async fn counters_count_exactly() {
    let (repo, h) = repo();
    let c = repo.storage().counters.clone();
    let s0 = c.snapshot();
    let (bid, _) = repo.put_blob(Bytes::from_static(b"counted")).await.unwrap();
    let d = c.snapshot() - s0;
    assert_eq!(d.objects_written, 1);
    assert_eq!(d.trees_written, 0);
    assert_eq!(d.blob_puts, 1);
    assert_eq!(d.meta_txns, 2, "strong redaction check, insert");

    // Re-putting an existing object writes nothing.
    let s1 = c.snapshot();
    repo.put_blob(Bytes::from_static(b"counted")).await.unwrap();
    let d = c.snapshot() - s1;
    assert_eq!((d.objects_written, d.blob_puts), (0, 0));

    let frame_len = h.blobs.raw(&repo.key(&bid, Area::Live)).unwrap().len() as u64;
    let s2 = c.snapshot();
    repo.get(&bid).await.unwrap();
    let d = c.snapshot() - s2;
    assert_eq!(
        d,
        CounterSnapshot {
            meta_txns: 1,
            meta_rows_read: 1,
            blob_gets: 1,
            blob_bytes_read: frame_len,
            blobs_read: 1,
            object_bytes_read: 7,
            ..CounterSnapshot::default()
        }
    );

    // Inline trees count as tree reads and touch no object storage.
    h.meta
        .put_object_row_raw(inline_row(Kind::Tree, EMPTY_TREE));
    let s3 = c.snapshot();
    assert_eq!(repo.get(&empty_tree_id()).await.unwrap(), EMPTY_TREE);
    let d = c.snapshot() - s3;
    assert_eq!((d.trees_read, d.blob_gets, d.object_bytes_read), (1, 0, 3));

    // Changesets are read from their row.
    let cs = opaque_cs_row(b"counted changeset", 1);
    repo.storage()
        .meta
        .commit_changeset(bundle(cs.clone(), vec![]))
        .await
        .unwrap();
    let s4 = c.snapshot();
    repo.get(&cs.id).await.unwrap();
    let d = c.snapshot() - s4;
    assert_eq!((d.changesets_read, d.blob_gets), (1, 0));

    // Opaque derived objects.
    let (did, _) = repo
        .put_encoded(Kind::Derived(0x10 + 1), Bytes::from_static(b"derived"))
        .await
        .unwrap();
    let s5 = c.snapshot();
    repo.get(&did).await.unwrap();
    assert_eq!((c.snapshot() - s5).derived_read, 1);
    c.reset();
    assert_eq!(c.snapshot(), CounterSnapshot::default());
}

#[tokio::test]
async fn decoded_tree_counts_and_survives_metadata_outage() {
    if skip_without_codec("decoded_tree_counts_and_survives_metadata_outage") {
        return;
    }
    let (repo, _h) = repo();
    let c = repo.storage().counters.clone();
    let enc = Bytes::from(tree_of_len(80));
    let (tid, _) = repo.put_encoded(Kind::Tree, enc).await.unwrap();
    let s0 = c.snapshot();
    let t1 = repo.get_tree(&tid).await.unwrap();
    let t2 = repo.get_tree(&tid).await.unwrap();
    assert_eq!(t1, t2);
    let d = c.snapshot() - s0.clone();
    assert_eq!(d.trees_read, 2, "cache hits count as tree reads");
    assert_eq!(d.meta_txns, 1, "the second read was served from the LRU");
    let meta = Bytes::from(test_meta("m").encode());
    let (mid, _) = repo.put_encoded(Kind::Meta, meta).await.unwrap();
    repo.get_meta(&mid).await.unwrap();
    let cs_bytes = scion_object::Changeset {
        parents: vec![],
        root_tree: tid,
        meta: mid,
        change_id: scion_core::ChangeId([3; 16]),
        delta: vec![],
        extensions: Extensions::new(),
    }
    .encode();
    let row = opaque_cs_row(&cs_bytes, 1);
    repo.storage()
        .meta
        .commit_changeset(bundle(row.clone(), vec![]))
        .await
        .unwrap();
    repo.get_changeset(&row.id).await.unwrap();

    // Metadata store down: every meta call fails, cached immutable reads keep working.
    let f = &repo.storage().faults;
    for m in ["get_objects", "get_changesets"] {
        f.arm(
            &format!("meta.{m}.before"),
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        );
    }
    assert_eq!(repo.get_tree(&tid).await.unwrap(), t1);
    assert_eq!(repo.get_meta(&mid).await.unwrap().message, b"m".to_vec());
    let (cs, crow) = repo.get_changeset(&row.id).await.unwrap();
    assert_eq!((cs.root_tree, crow), (tid, row));
    assert_code(repo.stat(&tid).await, ErrorCode::Unavailable);
    f.disarm_all();

    // A redacted Meta is no longer served from the cache.
    repo.storage()
        .meta
        .redact(RedactTxn {
            ids: vec![mid],
            reason: "r".to_owned(),
            actor: "a".to_owned(),
            at: t(1),
            purge_after: t(2),
        })
        .await
        .unwrap();
    assert_code(repo.get_meta(&mid).await, ErrorCode::Redacted);
}

fn redact_txn(ids: Vec<ObjectId>) -> RedactTxn {
    RedactTxn {
        ids,
        reason: "leak".to_owned(),
        actor: "github:admin".to_owned(),
        at: t(10),
        purge_after: t(20),
    }
}

fn audit() -> AuditRow {
    AuditRow {
        at: t(30),
        actor: "github:admin".to_owned(),
        action: "purge".to_owned(),
        target: "x".to_owned(),
        reason: "r".to_owned(),
        detail: String::new(),
    }
}

#[tokio::test]
async fn redacted_ids_are_blocked_and_bytes_move() {
    let (repo, h) = repo();
    let data = Bytes::from_static(b"secret");
    let (id, _) = repo.put_blob(data.clone()).await.unwrap();
    let live = repo.key(&id, Area::Live);
    let q = repo.key(&id, Area::Quarantine);
    repo.storage()
        .meta
        .redact(redact_txn(vec![id]))
        .await
        .unwrap();
    assert_code(repo.get(&id).await, ErrorCode::Redacted);
    assert_code(repo.get_frame(&id).await, ErrorCode::Redacted);
    assert_code(repo.put_blob(data.clone()).await, ErrorCode::Redacted);
    assert_code(
        repo.put_verified(&id, data.clone()).await,
        ErrorCode::Redacted,
    );

    // Crash between copy and delete; a re-run completes the move.
    let f = &repo.storage().faults;
    f.arm(points::QUARANTINE_AFTER_COPY, FaultAction::Crash, Some(1));
    assert!(repo.quarantine_bytes(&id).await.is_err());
    assert!(h.blobs.raw(&live).is_some() && h.blobs.raw(&q).is_some());
    f.reset();
    repo.quarantine_bytes(&id).await.unwrap();
    assert!(h.blobs.raw(&live).is_none());
    assert!(h.blobs.raw(&q).is_some());
    repo.quarantine_bytes(&id).await.unwrap();
    assert!(h.blobs.raw(&q).is_some(), "idempotent");

    // Unredact restores bytes and state.
    repo.storage().meta.unredact(&[id], audit()).await.unwrap();
    repo.restore_bytes(&id).await.unwrap();
    assert!(h.blobs.raw(&live).is_some() && h.blobs.raw(&q).is_none());
    assert_eq!(repo.get(&id).await.unwrap(), data);

    // Redact again and purge.
    repo.storage()
        .meta
        .redact(redact_txn(vec![id]))
        .await
        .unwrap();
    repo.quarantine_bytes(&id).await.unwrap();
    repo.purge_bytes(&id).await.unwrap();
    repo.storage()
        .meta
        .mark_purged(&[id], audit())
        .await
        .unwrap();
    assert!(h.blobs.keys().is_empty());
    assert_code(repo.get(&id).await, ErrorCode::Redacted);
    assert_code(repo.put_blob(data).await, ErrorCode::Redacted);
    repo.purge_bytes(&id).await.unwrap();

    // Inline objects have no bytes to move.
    h.meta
        .put_object_row_raw(inline_row(Kind::Meta, b"inline meta"));
    let mid = ObjectId::hash_encoded(Kind::Meta, b"inline meta");
    repo.quarantine_bytes(&mid).await.unwrap();
    repo.restore_bytes(&mid).await.unwrap();
}

#[tokio::test]
async fn reintroduction_is_blocked_even_when_stale_reads_lag() {
    // SPEC 13.6 / 5.4: redaction state is never taken from a stale read on the write path.
    let (repo, h) = repo_with(
        MemOptions {
            stale_lag_commits: 5,
            ..MemOptions::default()
        },
        ObjectRepoConfig::default(),
    );
    let data = Bytes::from_static(b"leaked secret");
    let (id, _) = repo.put_blob(data.clone()).await.unwrap();
    h.meta.advance_stale_view();
    repo.storage()
        .meta
        .redact(redact_txn(vec![id]))
        .await
        .unwrap();
    let stale = h
        .meta
        .get_objects(&[id], scion_store::Consistency::Stale)
        .await
        .unwrap();
    assert_eq!(
        stale[0].as_ref().map(|r| r.state),
        Some(ObjectState::Live),
        "the stale view still shows the row live"
    );
    assert_code(repo.put_blob(data.clone()).await, ErrorCode::Redacted);
    assert_code(repo.put_verified(&id, data).await, ErrorCode::Redacted);
}

#[tokio::test]
async fn object_storage_outage() {
    let (repo, h) = repo();
    let (id, _) = repo.put_blob(Bytes::from_static(b"stored")).await.unwrap();
    h.blobs.set_down(true);
    let e = assert_code(repo.get(&id).await, ErrorCode::Unavailable);
    assert_eq!(e.subcode(), Some(subcode::STORE_UNAVAILABLE));
    assert_code(
        repo.put_blob(Bytes::from_static(b"new")).await,
        ErrorCode::Unavailable,
    );
    assert_eq!(
        h.meta.table_counts().objects,
        1,
        "no row for the failed write"
    );
    // Inline objects still read (they live in the metadata store).
    let (did, _) = repo
        .put_encoded(Kind::Derived(0x11), Bytes::from_static(b"small"))
        .await
        .unwrap();
    assert_eq!(repo.get(&did).await.unwrap(), Bytes::from_static(b"small"));
    h.blobs.set_down(false);
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::from_static(b"stored"));
}

#[tokio::test]
async fn stat_many_mixes_kinds_and_misses() {
    let (repo, _h) = repo();
    let (b, _) = repo.put_blob(Bytes::from_static(b"a blob")).await.unwrap();
    let cs = opaque_cs_row(b"a changeset", 1);
    repo.storage()
        .meta
        .commit_changeset(bundle(cs.clone(), vec![]))
        .await
        .unwrap();
    let missing_cs = ObjectId::hash_encoded(Kind::Changeset, b"none");
    let missing_b = ObjectId::hash_blob(b"none");
    let got = repo
        .stat_many(&[cs.id, b, missing_b, missing_cs, b])
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(Option::is_some).collect::<Vec<_>>(),
        vec![true, true, false, false, true]
    );
    assert_eq!(got[0].as_ref().unwrap().kind, Kind::Changeset);
    assert_eq!(got[1].as_ref().unwrap().id, b);
    assert!(repo.stat_many(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn keys_follow_the_spec_layout() {
    let (repo, _h) = repo();
    let id = ObjectId::hash_blob(b"k");
    let repo_id = repo.storage().meta.repo_id().0;
    assert_eq!(
        repo.key(&id, Area::Live),
        format!("scion/r/{repo_id}/b/{}/{}", &id.hex()[..2], id.hex())
    );
    assert_eq!(
        repo.key(&id, Area::Quarantine),
        format!("scion/q/{repo_id}/b/{}/{}", &id.hex()[..2], id.hex())
    );
}

#[tokio::test]
async fn empty_blob_and_changeset_frames() {
    let (repo, h) = repo();
    let (id, _) = repo.put_blob(Bytes::new()).await.unwrap();
    assert_eq!(id, ObjectId::hash_blob(b""));
    assert!(h.blobs.raw(&repo.key(&id, Area::Live)).is_some());
    assert_eq!(repo.get(&id).await.unwrap(), Bytes::new());
    let cs = opaque_cs_row(b"framed changeset", 1);
    repo.storage()
        .meta
        .commit_changeset(bundle(cs.clone(), vec![]))
        .await
        .unwrap();
    let (frame, row) = repo.get_frame(&cs.id).await.unwrap();
    assert!(matches!(frame, StoredFrame::Raw(b) if b == cs.encoded));
    assert_eq!((row.kind, row.state), (Kind::Changeset, ObjectState::Live));
    // A corrupt changeset row is an integrity failure.
    let mut bad = cs.clone();
    bad.encoded = Bytes::from_static(b"tampered");
    h.meta.put_changeset_row_raw(bad);
    assert_code(repo.get(&cs.id).await, ErrorCode::Integrity);
    assert_eq!(repo.storage().counters.snapshot().integrity_failures, 1);
}
