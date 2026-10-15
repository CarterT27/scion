//! SPEC 19.7 / 7.5 UploadObjects: success, deduplication without leaks, every per-item error
//! (kind, digest, decoded form, size, redacted) and the call-level errors (P25).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;

use object_support::*;
use scion_core::fault::{FaultAction, points};
use scion_core::{ErrorCode, Kind, ObjectId};
use scion_object::{Canonical, Tree, TreeEntry};
use scion_proto::v1;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;

fn status(r: &v1::UploadResult) -> v1::ItemStatus {
    v1::ItemStatus::try_from(r.status).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_success_records_upload_and_deduplicates() {
    let srv = start().await;
    let ctx = srv.ctx();
    let c = srv.client(Some(&bob())).await;
    let a = b"blob a".to_vec();
    let b = b"blob b".to_vec();
    let ida = ObjectId::hash_blob(&a);
    let idb = ObjectId::hash_blob(&b);
    let resp = c
        .write()
        .upload_objects(tokio_stream::iter(vec![
            v1::UploadItem {
                id: ida.as_bytes().to_vec(),
                kind: 1,
                data: a.clone(),
            },
            v1::UploadItem {
                id: idb.as_bytes().to_vec(),
                kind: 1,
                data: b.clone(),
            },
        ]))
        .await
        .unwrap();
    assert!(epochs(resp.metadata()).is_some());
    let results = resp.into_inner().results;
    assert_eq!(results.len(), 2);
    for (r, id) in results.iter().zip([ida, idb]) {
        assert_eq!(r.id, id.as_bytes().to_vec());
        assert_eq!(status(r), v1::ItemStatus::Ok);
        assert!(r.error_code.is_empty());
    }
    assert_eq!(ctx.objects.get(&ida).await.unwrap().to_vec(), a);
    assert!(
        ctx.storage
            .meta
            .has_upload(&ida, "github:bob")
            .await
            .unwrap()
    );
    assert!(
        !ctx.storage
            .meta
            .has_upload(&ida, "github:alice")
            .await
            .unwrap()
    );

    // A repeat upload, by the same or another principal, looks exactly like the first.
    let (_, again) = upload_blob(&c, &a).await;
    assert_eq!(again, results[0]);
    let (_, other) = upload_blob(&srv.client(Some(&alice())).await, &a).await;
    assert_eq!(other, results[0]);
    assert!(
        ctx.storage
            .meta
            .has_upload(&ida, "github:alice")
            .await
            .unwrap()
    );

    // Trees may be uploaded too (validated in decoded form); an empty stream is fine.
    let tree = Tree {
        entries: vec![TreeEntry {
            name: scion_object::Name::new(b"a").unwrap(),
            mode: scion_core::Mode::File,
            id: ida,
            size: a.len() as u64,
        }],
        extensions: Default::default(),
    };
    let enc = tree.encode();
    let tid = ObjectId::hash_encoded(Kind::Tree, &enc);
    let r = upload(&c, vec![(tid.as_bytes().to_vec(), 2, enc.to_vec())])
        .await
        .unwrap();
    assert_eq!(status(&r[0]), v1::ItemStatus::Ok);
    assert!(upload(&c, vec![]).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_item_errors_do_not_fail_the_batch() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.limits.max_blob_bytes = 1024)),
        ..opts()
    })
    .await;
    let c = srv.client(Some(&alice())).await;
    let good = b"good".to_vec();
    let gid = ObjectId::hash_blob(&good);
    let redacted = b"secret".to_vec();
    let rid = ObjectId::hash_blob(&redacted);
    srv.ctx()
        .objects
        .put_blob(bytes::Bytes::from(redacted.clone()))
        .await
        .unwrap();
    srv.ctx()
        .redactor
        .redact(&[rid], "leak", &admin(), None)
        .await
        .unwrap();
    srv.ctx().redactions.refresh().await.unwrap();
    let big = vec![b'x'; 2048];
    let bid = ObjectId::hash_blob(&big);
    let bad_tree = b"not a tree".to_vec();
    let btid = ObjectId::hash_encoded(Kind::Tree, &bad_tree);
    let cs_id = ObjectId::hash_encoded(Kind::Changeset, b"cs");
    let items = vec![
        (gid.as_bytes().to_vec(), 2, good.clone()), // kind mismatch
        (gid.as_bytes().to_vec(), 1, b"other".to_vec()), // digest mismatch
        (rid.as_bytes().to_vec(), 1, redacted.clone()), // redacted
        (bid.as_bytes().to_vec(), 1, big),          // too large
        (btid.as_bytes().to_vec(), 2, bad_tree),    // invalid decoded form
        (cs_id.as_bytes().to_vec(), 3, b"cs".to_vec()), // changesets cannot be uploaded
        (vec![1, 2, 3], 1, good.clone()),           // malformed id
        (Vec::new(), 1, good.clone()),              // missing id
        (gid.as_bytes().to_vec(), 999, good.clone()), // kind out of range
        (gid.as_bytes().to_vec(), 1, good.clone()), // ok
    ];
    let r = upload(&c, items).await.unwrap();
    let got: Vec<(v1::ItemStatus, String)> = r
        .iter()
        .map(|r| (status(r), r.error_code.clone()))
        .collect();
    let u = v1::ItemStatus::Unspecified;
    assert_eq!(
        got,
        vec![
            (u, "INVALID_ARGUMENT".to_owned()),
            (u, "INTEGRITY".to_owned()),
            (v1::ItemStatus::Redacted, "REDACTED".to_owned()),
            (u, "TOO_LARGE".to_owned()),
            (u, "INVALID_ARGUMENT".to_owned()),
            (u, "INVALID_ARGUMENT".to_owned()),
            (u, "INVALID_ARGUMENT".to_owned()),
            (u, "INVALID_ARGUMENT".to_owned()),
            (u, "INVALID_ARGUMENT".to_owned()),
            (v1::ItemStatus::Ok, String::new()),
        ]
    );
    assert!(r.iter().take(9).all(|r| !r.message.is_empty()));
    // Nothing was stored for the failed items; the client's bad digest is no server alert.
    assert!(srv.ctx().objects.stat(&bid).await.unwrap().is_none());
    assert!(srv.ctx().objects.stat(&btid).await.unwrap().is_none());
    assert_eq!(
        srv.ctx()
            .metrics
            .integrity_failures
            .with_label_values(&["server"])
            .get(),
        0
    );
    // No upload record for a rejected redacted id.
    assert!(
        !srv.ctx()
            .storage
            .meta
            .has_upload(&rid, "github:alice")
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_call_level_errors() {
    let srv = start().await;
    let err = srv
        .client(None)
        .await
        .write()
        .upload_objects(tokio_stream::iter(vec![v1::UploadItem {
            id: ObjectId::hash_blob(b"x").as_bytes().to_vec(),
            kind: 1,
            data: b"x".to_vec(),
        }]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(epochs(err.metadata()).is_some());

    // Object storage down: the call fails UNAVAILABLE (retryable), nothing recorded.
    let c = srv.client(Some(&alice())).await;
    srv.mem.as_ref().unwrap().blobs.set_down(true);
    let content = b"while down".to_vec();
    let id = ObjectId::hash_blob(&content);
    let err = upload(&c, vec![(id.as_bytes().to_vec(), 1, content.clone())])
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Unavailable);
    srv.mem.as_ref().unwrap().blobs.set_down(false);
    assert!(
        !srv.ctx()
            .storage
            .meta
            .has_upload(&id, "github:alice")
            .await
            .unwrap()
    );
    // Metadata down at record_upload: UNAVAILABLE too.
    srv.storage.faults.arm(
        &points::meta_before("record_upload"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let err = upload(&c, vec![(id.as_bytes().to_vec(), 1, content.clone())])
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Unavailable);
    srv.storage.faults.disarm_all();
    let (_, r) = upload_blob(&c, &content).await;
    assert_eq!(status(&r), v1::ItemStatus::Ok);
}

/// SPEC 12.5 / 12.6: a Meta has no `object_paths` rows and is visible to every repository
/// reader, so an uploaded (unreferenced) Meta would be readable by everyone. Metas are
/// server-built (CreateChangeset), so uploads of them are rejected and store nothing.
#[tokio::test(flavor = "multi_thread")]
async fn meta_uploads_are_rejected_and_never_readable() {
    let srv = start().await;
    let meta = scion_object::Meta {
        author: b"Mallory <m@example.com>".to_vec(),
        author_time: scion_object::Timestamp {
            seconds: 1,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"github:bob".to_vec(),
        message: b"uploaded, never referenced".to_vec(),
        extensions: scion_object::Extensions::new(),
    };
    let enc = meta.encode();
    let mid = meta.id();
    assert_eq!(mid.kind(), Kind::Meta);
    let b = srv.client(Some(&bob())).await;
    let r = upload(
        &b,
        vec![(mid.as_bytes().to_vec(), Kind::Meta.byte().into(), enc)],
    )
    .await
    .unwrap();
    assert_eq!(status(&r[0]), v1::ItemStatus::Unspecified);
    assert_eq!(r[0].error_code, "INVALID_ARGUMENT");
    assert!(srv.ctx().objects.stat(&mid).await.unwrap().is_none());
    assert!(
        !srv.ctx()
            .storage
            .meta
            .has_upload(&mid, "github:bob")
            .await
            .unwrap()
    );
    for p in [alice(), bob(), carol(), admin()] {
        let c = srv.client(Some(&p)).await;
        assert_eq!(status_of(&c, &mid).await, v1::ItemStatus::NotFound, "{p}");
    }
    assert_eq!(has(&b, &[mid]).await, vec![false]);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_batches_preserve_order_and_reduce_metadata_writes() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.redaction.propagation_bound_ms = 600_000)),
        ..opts()
    })
    .await;
    let c = srv.client(Some(&alice())).await;
    let items: Vec<_> = (0_u32..130)
        .map(|n| {
            let data = n.to_be_bytes().to_vec();
            (ObjectId::hash_blob(&data).as_bytes().to_vec(), 1, data)
        })
        .collect();
    upload(&c, vec![]).await.unwrap();
    let before = srv.storage.counters.snapshot();
    let results = upload(&c, items.clone()).await.unwrap();
    let delta = srv.storage.counters.snapshot() - before;
    // One strong pre-check and insert per object, plus three ownership batch transactions.
    assert_eq!(delta.meta_txns, 2 * 130 + 3);
    for (result, (raw_id, _, data)) in results.iter().zip(&items) {
        assert_eq!(&result.id, raw_id);
        assert_eq!(status(result), v1::ItemStatus::Ok);
        let id = ObjectId::hash_blob(data);
        assert!(
            srv.storage
                .meta
                .has_upload(&id, "github:alice")
                .await
                .unwrap()
        );
    }
    let before = srv.storage.counters.snapshot();
    assert_eq!(upload(&c, items).await.unwrap(), results);
    let delta = srv.storage.counters.snapshot() - before;
    assert_eq!(delta.meta_txns, 130 + 3);
    assert_eq!(delta.blob_puts, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_batch_failure_never_records_ownership() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let items: Vec<_> = [b"batch first".to_vec(), b"batch second".to_vec()]
        .into_iter()
        .map(|data| (ObjectId::hash_blob(&data).as_bytes().to_vec(), 1, data))
        .collect();
    srv.storage.faults.arm(
        &points::meta_before("record_upload"),
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let err = upload(&c, items.clone()).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Unavailable);
    srv.storage.faults.disarm_all();
    for (raw_id, _, data) in &items {
        let id = ObjectId::hash_blob(data);
        assert_eq!(raw_id, &id.as_bytes().to_vec());
        assert!(srv.ctx().objects.stat(&id).await.unwrap().is_some());
        assert!(
            !srv.storage
                .meta
                .has_upload(&id, "github:alice")
                .await
                .unwrap()
        );
    }
    assert!(
        upload(&c, items)
            .await
            .unwrap()
            .iter()
            .all(|r| status(r) == v1::ItemStatus::Ok)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_batches_are_bounded_by_payload_bytes() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.redaction.propagation_bound_ms = 600_000)),
        ..opts()
    })
    .await;
    let c = srv.client(Some(&alice())).await;
    // Two 3 MiB objects fit together; the third starts a new batch. A 9 MiB object
    // must run alone, and the following small item starts another batch.
    let items: Vec<_> = [
        (1, 3 * 1024 * 1024),
        (2, 3 * 1024 * 1024),
        (3, 3 * 1024 * 1024),
        (4, 9 * 1024 * 1024),
        (5, 1),
    ]
    .into_iter()
    .map(|(byte, len)| {
        let data = vec![byte; len];
        (ObjectId::hash_blob(&data).as_bytes().to_vec(), 1, data)
    })
    .collect();
    upload(&c, vec![]).await.unwrap();
    let before = srv.storage.counters.snapshot();
    let results = upload(&c, items.clone()).await.unwrap();
    let delta = srv.storage.counters.snapshot() - before;
    assert_eq!(delta.meta_txns, 2 * 5 + 4);
    for (result, (raw_id, _, data)) in results.iter().zip(items) {
        assert_eq!(result.id, raw_id);
        assert_eq!(status(result), v1::ItemStatus::Ok);
        let id = ObjectId::hash_blob(&data);
        assert_eq!(srv.ctx().objects.get(&id).await.unwrap().as_ref(), data);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_batch_ownership_item_error_is_reported_per_item() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let data = b"ownership item error".to_vec();
    let id = ObjectId::hash_blob(&data);
    srv.storage.faults.arm(
        &points::meta_before("record_upload"),
        FaultAction::Error(ErrorCode::InvalidArgument),
        Some(1),
    );
    let results = upload(&c, vec![(id.as_bytes().to_vec(), 1, data)])
        .await
        .unwrap();
    assert_eq!(results[0].error_code, "INVALID_ARGUMENT");
    assert_eq!(status(&results[0]), v1::ItemStatus::Unspecified);
    assert!(
        !srv.storage
            .meta
            .has_upload(&id, "github:alice")
            .await
            .unwrap()
    );
}
