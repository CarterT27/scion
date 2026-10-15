//! SPEC 19.7 for the object RPCs (GetObjects, HasObjects, ResolveRef) and CheckAccess: success,
//! each error code, authentication failure, mixed batches, ranges, encodings, metrics, and the
//! SPEC 16.1 class 7 integrity alert (P25).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;

use object_support::*;
use scion_core::fault::{FaultAction, points};
use scion_core::{ErrorCode, ObjectId, Tier, subcode as sc};
use scion_proto::v1;
use scion_store::Area;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn get_objects_serves_zstd_frames_inline_raw_and_counts_bytes() {
    let srv = start().await;
    let ctx = srv.ctx();
    let content = b"hello world\n".repeat(100);
    let cs = commit(&srv, &[("docs/readme.txt", &content)]).await;
    let blob = cs.blobs["docs/readme.txt"];
    let c = srv.client(Some(&alice())).await;

    let resp = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(&[blob]),
            offset: None,
            length: None,
        })
        .await
        .unwrap();
    let mut s = resp.into_inner();
    let mut frames = Vec::new();
    while let Some(ch) = s.message().await.unwrap() {
        frames.push(ch);
    }
    let trailers = s.trailers().await.unwrap().unwrap();
    let e = epochs(&trailers).expect("epoch trailers on the stream");
    assert_eq!(e.tier, Tier::Server);
    assert_eq!(frames.len(), 1);
    let ch = &frames[0];
    assert_eq!(ch.status, v1::ItemStatus::Ok as i32);
    assert_eq!(ch.encoding, v1::Encoding::Zstd as i32);
    assert!(ch.r#final);
    assert_eq!(ch.total_size, content.len() as u64);
    assert_eq!(ch.access_class, v1::AccessClass::Root as i32);
    // The stored frame is passed through unmodified.
    let stored = srv
        .mem
        .as_ref()
        .unwrap()
        .blobs
        .raw(&ctx.objects.key(&blob, Area::Live))
        .unwrap();
    assert_eq!(ch.data, stored.to_vec());
    assert_eq!(zstd::stream::decode_all(&ch.data[..]).unwrap(), content);
    assert_eq!(
        ctx.metrics
            .object_bytes_served
            .with_label_values(&["server", "zstd"])
            .get(),
        stored.len() as u64
    );

    // Trees are inline: RAW, authorized through their directory path (root at "").
    for tree in [cs.root_tree] {
        let (items, err) = get_objects(&c, &[tree], None, None).await.unwrap();
        assert!(err.is_none());
        assert_eq!(items[0].status, v1::ItemStatus::Ok);
        assert_eq!(items[0].encoding, v1::Encoding::Raw);
        assert_eq!(ObjectId::hash_encoded(tree.kind(), &items[0].data), tree);
    }
    let docs = ctx
        .history
        .lookup(
            &cs.root_tree,
            &scion_object::RepoPath::parse(b"docs").unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
        .id;
    let (items, _) = get_objects(&c, &[docs], None, None).await.unwrap();
    assert_eq!(items[0].status, v1::ItemStatus::Ok);
    assert_eq!(items[0].access_class, v1::AccessClass::Root);
    assert!(
        ctx.metrics
            .object_bytes_served
            .with_label_values(&["server", "raw"])
            .get()
            > 0
    );
    // Metas are visible to repository readers (SPEC 12.6).
    let (items, _) = get_objects(&c, &[cs.meta], None, None).await.unwrap();
    assert_eq!(items[0].status, v1::ItemStatus::Ok);
    assert_eq!(
        status_of(&srv.client(Some(&dave())).await, &cs.meta).await,
        v1::ItemStatus::Denied
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_objects_range_reads_and_large_objects() {
    let srv = start().await;
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let cs = commit(&srv, &[("big.bin", &big), ("small.txt", b"0123456789")]).await;
    let c = srv.client(Some(&alice())).await;
    let small = cs.blobs["small.txt"];

    let (items, err) = get_objects(&c, &[small], Some(3), Some(4)).await.unwrap();
    assert!(err.is_none());
    assert_eq!(items[0].encoding, v1::Encoding::Raw);
    assert_eq!(items[0].data, b"3456");
    assert_eq!(items[0].first_offset, 3);
    assert_eq!(items[0].total_size, 10);
    // Past the end, open-ended and zero-length ranges.
    let (items, _) = get_objects(&c, &[small], Some(20), None).await.unwrap();
    assert_eq!(items[0].data, b"");
    let (items, _) = get_objects(&c, &[small], Some(8), None).await.unwrap();
    assert_eq!(items[0].data, b"89");
    let (items, _) = get_objects(&c, &[small], None, Some(0)).await.unwrap();
    assert_eq!(items[0].status, v1::ItemStatus::Ok);
    assert!(items[0].data.is_empty());

    // A zero-length probe answers from the objects row without reading content.
    let blob = cs.blobs["big.bin"];
    let before = srv.ctx().storage.counters.snapshot();
    let (items, _) = get_objects(&c, &[blob], None, Some(0)).await.unwrap();
    let after = srv.ctx().storage.counters.snapshot();
    assert_eq!(items[0].status, v1::ItemStatus::Ok);
    assert!(items[0].data.is_empty());
    assert_eq!(items[0].total_size, big.len() as u64);
    assert_eq!(after.blob_gets, before.blob_gets);
    assert_eq!(after.blob_bytes_read, before.blob_bytes_read);

    // A multi-megabyte object spans several chunks and reassembles exactly.
    let (items, _) = get_objects(&c, &[blob, small], Some(0), None)
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert!(items[0].chunks >= 3);
    assert_eq!(items[0].data, big);
    assert_eq!(items[1].data, b"0123456789");
    let content = read_ok(&c, &blob).await;
    assert_eq!(content, big);
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_batch_returns_each_status_without_failing() {
    let srv = start().await;
    let cs = commit(
        &srv,
        &[
            ("pub/a.txt", b"public a"),
            ("secrets/key.txt", b"secret key"),
            ("pub/b.txt", b"to be redacted"),
        ],
    )
    .await;
    let ok = cs.blobs["pub/a.txt"];
    let denied = cs.blobs["secrets/key.txt"];
    let redacted = cs.blobs["pub/b.txt"];
    let missing = ObjectId::hash_blob(b"never stored");
    srv.ctx()
        .redactor
        .redact(&[redacted], "test", &admin(), None)
        .await
        .unwrap();
    srv.ctx().redactions.refresh().await.unwrap();

    let c = srv.client(Some(&alice())).await;
    let (items, err) = get_objects(&c, &[missing, ok, denied, redacted, ok], None, None)
        .await
        .unwrap();
    assert!(err.is_none(), "{err:?}");
    let got: Vec<(ObjectId, v1::ItemStatus)> = items.iter().map(|i| (i.id, i.status)).collect();
    assert_eq!(
        got,
        vec![
            (missing, v1::ItemStatus::NotFound),
            (ok, v1::ItemStatus::Ok),
            (denied, v1::ItemStatus::Denied),
            (redacted, v1::ItemStatus::Redacted),
            (ok, v1::ItemStatus::Ok),
        ]
    );
    assert_eq!(items[1].content(), b"public a");
    for i in [0, 2, 3] {
        assert!(items[i].data.is_empty());
        assert_eq!(items[i].access_class, v1::AccessClass::Unspecified);
    }
    // The security group reads the secret with the restricted class; a principal who reads
    // nothing is denied everything that exists.
    let (items, _) = get_objects(&srv.client(Some(&carol())).await, &[denied], None, None)
        .await
        .unwrap();
    assert_eq!(items[0].status, v1::ItemStatus::Ok);
    assert_eq!(items[0].access_class, v1::AccessClass::Restricted);
    let d = srv.client(Some(&dave())).await;
    assert_eq!(status_of(&d, &ok).await, v1::ItemStatus::Denied);
    assert_eq!(status_of(&d, &missing).await, v1::ItemStatus::NotFound);
    // An empty request is an empty stream.
    let (items, err) = get_objects(&c, &[], None, None).await.unwrap();
    assert!(items.is_empty() && err.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_objects_rechecks_redaction_of_prefetched_payload() {
    let srv = start().await;
    let cs = commit(&srv, &[("a.txt", b"a"), ("b.txt", b"b")]).await;
    let ctx = srv.ctx();
    let before = ctx.storage.faults.hits("blob.get.after");
    ctx.storage.faults.arm(
        "blob.get.before",
        FaultAction::Delay(Duration::from_secs(5)),
        Some(1),
    );
    let c = srv.client(Some(&alice())).await;
    let response = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(&[cs.blobs["a.txt"], cs.blobs["b.txt"]]),
            offset: None,
            length: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while ctx.storage.faults.hits("blob.get.after") == before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second payload is prefetched while first is stalled");
    let redacted = cs.blobs["b.txt"];
    ctx.redactor
        .redact(&[redacted], "prefetch race", &admin(), None)
        .await
        .unwrap();
    ctx.redactions.refresh().await.unwrap();
    let mut stream = response.into_inner();
    let first = stream.message().await.unwrap().unwrap();
    assert_eq!(first.id, ids(&[cs.blobs["a.txt"]])[0]);
    assert_eq!(first.status, v1::ItemStatus::Ok as i32);
    let second = stream.message().await.unwrap().unwrap();
    assert_eq!(second.id, ids(&[redacted])[0]);
    assert_eq!(second.status, v1::ItemStatus::Redacted as i32);
    assert!(second.data.is_empty());
    assert!(stream.message().await.unwrap().is_none());
    assert_eq!(
        ctx.metrics
            .object_bytes_served
            .with_label_values(&["server", "zstd"])
            .get(),
        ctx.objects
            .stat(&cs.blobs["a.txt"])
            .await
            .unwrap()
            .unwrap()
            .stored_size,
        "discarded prefetched bytes must not count as served",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_objects_starts_multiple_reads_before_first_completes() {
    let srv = start().await;
    let cs = commit(&srv, &[("a.txt", b"a"), ("b.txt", b"b")]).await;
    let ctx = srv.ctx();
    let before = ctx.storage.faults.hits("blob.get.before");
    ctx.storage.faults.arm(
        "blob.get.before",
        FaultAction::Delay(Duration::from_secs(60)),
        None,
    );
    let c = srv.client(Some(&alice())).await;
    let response = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(&[cs.blobs["a.txt"], cs.blobs["b.txt"]]),
            offset: None,
            length: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while ctx.storage.faults.hits("blob.get.before") < before + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both reads start while the first remains stalled");
    drop(response);
    ctx.storage.faults.disarm_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn get_objects_errors() {
    let srv = start().await;
    let cs = commit(&srv, &[("a.txt", b"aaa"), ("b.txt", b"bbb")]).await;
    let a = cs.blobs["a.txt"];

    // Authentication failure.
    let err = srv
        .client(None)
        .await
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(&[a]),
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(epochs(err.metadata()).is_some());

    // Malformed id.
    let c = srv.client(Some(&alice())).await;
    let err = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![vec![1, 2, 3]],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);
    assert_eq!(subcode(&err).as_deref(), Some(sc::BAD_ID_TEXT));

    // Object storage down: UNAVAILABLE (retryable).
    srv.mem.as_ref().unwrap().blobs.set_down(true);
    let (items, err) = get_objects(&c, &[a], None, None).await.unwrap();
    assert!(items.is_empty());
    let err = err.expect("stream fails");
    assert_eq!(code(&err), ErrorCode::Unavailable);
    assert!(scion_proto::error_from_status(&err).retryable());
    srv.mem.as_ref().unwrap().blobs.set_down(false);
    assert_eq!(read_ok(&c, &a).await, b"aaa");
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_server_copy_is_an_integrity_item_with_alert() {
    let srv = start().await;
    let ctx = srv.ctx();
    let cs = commit(
        &srv,
        &[
            ("a.txt", b"first"),
            ("b.txt", b"corrupt me"),
            ("c.txt", b"third"),
        ],
    )
    .await;
    let (a, b, c3) = (cs.blobs["a.txt"], cs.blobs["b.txt"], cs.blobs["c.txt"]);
    srv.mem
        .as_ref()
        .unwrap()
        .blobs
        .corrupt(&ctx.objects.key(&b, Area::Live));
    let before = ctx.storage.counters.snapshot().integrity_failures;
    let c = srv.client(Some(&alice())).await;
    let (items, err) = get_objects(&c, &[a, b, c3], None, None).await.unwrap();
    // Every other item is served; the corrupt one fails with INTEGRITY.
    let got: Vec<ObjectId> = items.iter().map(|i| i.id).collect();
    assert_eq!(got, vec![a, c3]);
    assert!(items.iter().all(|i| i.status == v1::ItemStatus::Ok));
    let err = err.expect("INTEGRITY ends the stream");
    assert_eq!(code(&err), ErrorCode::Integrity);
    assert_eq!(err.code(), tonic::Code::DataLoss);
    assert!(err.message().contains(&b.to_text()), "{}", err.message());
    assert!(epochs(err.metadata()).is_some());
    assert_eq!(
        ctx.metrics
            .integrity_failures
            .with_label_values(&["server"])
            .get(),
        1
    );
    assert_eq!(
        ctx.storage.counters.snapshot().integrity_failures,
        before + 1
    );
    assert!(
        ctx.metrics
            .render()
            .contains("scion_integrity_failures_total{tier=\"server\"} 1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn has_objects_success_errors_and_auth() {
    let srv = start().await;
    let cs = commit(&srv, &[("x/a.txt", b"a"), ("secrets/s.txt", b"s")]).await;
    let a = cs.blobs["x/a.txt"];
    let s = cs.blobs["secrets/s.txt"];
    let missing = ObjectId::hash_blob(b"missing");
    let c = srv.client(Some(&alice())).await;
    let resp = c
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: ids(&[a, s, missing, cs.root_tree]),
        })
        .await
        .unwrap();
    assert!(epochs(resp.metadata()).is_some());
    assert_eq!(resp.into_inner().present, vec![true, false, false, true]);
    assert_eq!(
        has(&srv.client(Some(&carol())).await, &[s]).await,
        vec![true]
    );

    let err = c
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![b"bad".to_vec()],
        })
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);
    let err = srv
        .client(None)
        .await
        .objects()
        .has_objects(v1::HasObjectsRequest { ids: ids(&[a]) })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);

    // Metadata outage: UNAVAILABLE.
    let faults = srv.storage.faults.clone();
    // A failed preflight must retain the original path and its outage behavior.
    faults.arm(
        &points::meta_before("get_objects"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    faults.arm(
        &points::meta_before("object_paths"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let err = c
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: ids(&[missing]),
        })
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::Unavailable);
    faults.disarm_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_ref_current_versions_and_errors() {
    let srv = start().await;
    let ctx = srv.ctx();
    let main = scion_object::RefName::main();
    let root = ctx.history.resolve_ref(&main, None).await.unwrap();
    let cs = commit(&srv, &[("f.txt", b"f")]).await;
    ctx.history
        .update_ref(
            scion_history::UpdateRefRequest {
                name: main.clone(),
                expected_version: root.version,
                new_target: cs.changeset,
                allow_non_fast_forward: false,
            },
            &alice(),
            &scion_history::AllowAll,
        )
        .await
        .unwrap();
    let c = srv.client(Some(&dave())).await; // refs are not path-scoped
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap();
    assert!(epochs(r.metadata()).is_some());
    let r = r.into_inner();
    assert_eq!(r.target, cs.changeset.as_bytes().to_vec());
    assert_eq!(r.version, root.version + 1);
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: Some(root.version),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.target, root.target.as_bytes().to_vec());
    assert_eq!(r.version, root.version);

    for (name, at, want) in [
        ("nope", None, ErrorCode::NotFound),
        ("main", Some(99), ErrorCode::NotFound),
        ("", None, ErrorCode::InvalidArgument),
        ("bad..name", None, ErrorCode::InvalidArgument),
    ] {
        let err = c
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: name.into(),
                at_version: at,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&err), want, "{name:?}");
    }
    let err = srv
        .client(None)
        .await
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread")]
async fn check_access_answers_for_the_subject() {
    let srv = start().await;
    let cs = commit(&srv, &[("pub/a.txt", b"a"), ("secrets/b.txt", b"b")]).await;
    let a = cs.blobs["pub/a.txt"];
    let b = cs.blobs["secrets/b.txt"];
    let missing = ObjectId::hash_blob(b"missing");
    // The caller is a daemon-like service; the subject decides.
    let caller = srv.client(Some(&dave())).await;
    let ask = |subject: String, want: Vec<ObjectId>| {
        let caller = caller.clone();
        async move {
            caller
                .auth()
                .check_access(v1::CheckAccessRequest {
                    subject_token: subject,
                    object_ids: ids(&want),
                })
                .await
        }
    };
    let r = ask(srv.token_for(&alice()), vec![a, b, missing, cs.root_tree])
        .await
        .unwrap();
    assert!(epochs(r.metadata()).is_some());
    let r = r.into_inner();
    assert_eq!(r.allowed, vec![true, false, false, true]);
    use v1::AccessClass as A;
    assert_eq!(
        r.access_class,
        vec![
            A::Root as i32,
            A::Unspecified as i32,
            A::Unspecified as i32,
            A::Root as i32
        ]
    );
    // A principal who reads only /secrets gets RESTRICTED (never ROOT: that would let a
    // daemon confirm it as a root reader).
    let r = ask(srv.token_for(&sec()), vec![a, b])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.allowed, vec![false, true]);
    assert_eq!(
        r.access_class,
        vec![A::Unspecified as i32, A::Restricted as i32]
    );

    // Subject token problems.
    let err = ask("garbage".into(), vec![a]).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let expired = srv
        .keyring
        .issue(
            &alice(),
            Duration::from_secs(1),
            std::time::SystemTime::now() - Duration::from_secs(3600),
        )
        .unwrap()
        .0;
    let err = ask(expired, vec![a]).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let err = ask(String::new(), vec![a]).await.unwrap_err();
    assert_eq!(subcode(&err).as_deref(), Some(sc::MISSING_FIELD));
    let mut bad = ids(&[a]);
    bad.push(vec![0; 3]);
    let err = caller
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: srv.token_for(&alice()),
            object_ids: bad,
        })
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::InvalidArgument);
    // Caller authentication failure.
    let err = srv
        .client(None)
        .await
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: srv.token_for(&alice()),
            object_ids: ids(&[a]),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread")]
async fn has_objects_batches_unseen_blobs_in_order() {
    let srv =
        scion_server::testing::InProcessServer::start(scion_server::testing::ServerTestOpts {
            tweak: Some(std::sync::Arc::new(|c| {
                c.redaction.propagation_bound_ms = 600_000
            })),
            ..opts()
        })
        .await;
    let c = srv.client(Some(&alice())).await;
    has(&c, &[]).await;
    let mut requested: Vec<_> = (0_u32..1003)
        .map(|n| ObjectId::hash_blob(&n.to_be_bytes()))
        .collect();
    let owned = requested[17];
    requested.extend([owned, requested[1]]);
    srv.storage
        .meta
        .record_upload(&owned, "github:alice", std::time::SystemTime::now())
        .await
        .unwrap();
    let before = srv.storage.counters.snapshot();
    let present = has(&c, &requested).await;
    assert_eq!((srv.storage.counters.snapshot() - before).meta_txns, 4);
    assert_eq!(
        present,
        requested.iter().map(|id| *id == owned).collect::<Vec<_>>()
    );
    assert_eq!(
        has(&srv.client(Some(&bob())).await, &[owned, owned]).await,
        vec![false, false]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn has_objects_batch_preflight_failures_fall_back() {
    let srv = start().await;
    let c = srv.client(Some(&alice())).await;
    let missing = ObjectId::hash_blob(b"absent fallback");
    for operation in ["get_objects", "has_uploads"] {
        srv.storage.faults.arm(
            &points::meta_before(operation),
            FaultAction::Error(ErrorCode::Unavailable),
            Some(1),
        );
        assert_eq!(has(&c, &[missing, missing]).await, vec![false, false]);
        srv.storage.faults.disarm_all();
    }
    let cs = commit(&srv, &[("readable", b"cached")]).await;
    let live = cs.blobs["readable"];
    assert_eq!(has(&c, &[live]).await, vec![true]);
    srv.mem.as_ref().unwrap().meta.set_fail_txn_every(Some(1));
    // The preflight fails, but the existing authorization cache remains available.
    assert_eq!(has(&c, &[live, live]).await, vec![true, true]);
    srv.mem.as_ref().unwrap().meta.set_fail_txn_every(None);
}

/// GetObjects authorizes and stats ids in batches: metadata transactions grow with the
/// number of batches, not with the number of objects.
#[tokio::test(flavor = "multi_thread")]
async fn get_objects_batches_metadata_reads() {
    let srv = start().await;
    let names: Vec<String> = (0..600).map(|i| format!("d/f{i}.txt")).collect();
    let contents: Vec<Vec<u8>> = (0..600)
        .map(|i| format!("file {i}\n").into_bytes())
        .collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&contents)
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let cs = commit(&srv, &files).await;
    let ids: Vec<ObjectId> = names.iter().map(|n| cs.blobs[n.as_str()]).collect();
    let c = srv.client(Some(&alice())).await;
    srv.ctx().authz.clear_cache();
    let before = srv.ctx().storage.counters.snapshot();
    let (items, err) = get_objects(&c, &ids, None, None).await.unwrap();
    let delta = srv.ctx().storage.counters.snapshot() - before;
    assert!(err.is_none());
    assert_eq!(items.len(), 600);
    for (item, want) in items.iter().zip(&contents) {
        assert_eq!(item.status, v1::ItemStatus::Ok);
        assert_eq!(&item.content(), want);
    }
    // 3 batches of at most 256 ids: one `objects` read and one `object_paths` read each.
    assert!(
        delta.meta_txns <= 10,
        "{} metadata transactions for 600 objects",
        delta.meta_txns
    );
}
