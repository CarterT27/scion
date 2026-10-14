//! FakeUpstream ObjectService behaviour (PLAN 2.21; SPEC 7.3, 19.7): a batch mixing ok,
//! not_found, denied and redacted; access classes; zstd and raw frames; range reads; stream kill
//! and resume by offset; corruption; HasObjects; TLS and Unix-socket modes; epoch trailers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ErrorCode, ObjectId, Tier};
use scion_proto::{Epochs, ScionClient, error_from_status, v1};
use scion_testkit::{FakeUpstream, FakeUpstreamOpts};
use std::collections::BTreeMap;
use std::sync::Arc;

fn opts(tls: bool) -> FakeUpstreamOpts {
    FakeUpstreamOpts {
        tls,
        principal_of: Arc::new(|t| t.strip_prefix("tok-").map(|p| format!("github:{p}"))),
        public_keys: vec![("k1".into(), [7; 32])],
    }
}

async fn client(up: &FakeUpstream, who: &str) -> ScionClient {
    ScionClient::connect(&up.endpoint(), up.tls(), Some(format!("tok-{who}")))
        .await
        .unwrap()
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Item {
    status: i32,
    encoding: i32,
    data: Vec<u8>,
    total: u64,
    access: i32,
    chunks: usize,
    finals: usize,
    base: u64,
}

/// Fetch and reassemble per id (chunks must be contiguous by offset). Returns the items in
/// arrival order of first chunk and the terminal error, if any.
async fn fetch(
    c: &ScionClient,
    ids: &[ObjectId],
    offset: Option<u64>,
    length: Option<u64>,
) -> (Vec<(ObjectId, Item)>, Option<tonic::Status>) {
    let mut s = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
            offset,
            length,
        })
        .await
        .unwrap()
        .into_inner();
    let mut order: Vec<ObjectId> = Vec::new();
    let mut items: BTreeMap<ObjectId, Item> = BTreeMap::new();
    let err = loop {
        match s.message().await {
            Ok(Some(ch)) => {
                let id = ObjectId::from_bytes(&ch.id).unwrap();
                if !items.contains_key(&id) {
                    order.push(id);
                }
                let it = items.entry(id).or_default();
                if it.chunks == 0 {
                    it.base = ch.offset;
                }
                assert_eq!(
                    ch.offset,
                    it.base + it.data.len() as u64,
                    "chunks are contiguous"
                );
                it.status = ch.status;
                it.encoding = ch.encoding;
                it.total = ch.total_size;
                it.access = ch.access_class;
                it.data.extend_from_slice(&ch.data);
                it.chunks += 1;
                it.finals += usize::from(ch.r#final);
            }
            Ok(None) => break None,
            Err(e) => break Some(e),
        }
    };
    let out = order
        .into_iter()
        .map(|id| (id, items.remove(&id).unwrap()))
        .collect();
    (out, err)
}

fn blob(content: &[u8]) -> (ObjectId, Bytes) {
    (
        ObjectId::hash_blob(content),
        Bytes::copy_from_slice(content),
    )
}

/// Decompress (when zstd) and verify the digest, as a receiver must (SPEC 7.3).
fn verified(id: &ObjectId, it: &Item) -> Option<Vec<u8>> {
    let raw = if it.encoding == v1::Encoding::Zstd as i32 {
        zstd::decode_all(it.data.as_slice()).ok()?
    } else {
        it.data.clone()
    };
    (ObjectId::hash_blob(&raw) == *id).then_some(raw)
}

async fn batch_suite(up: &FakeUpstream) {
    let (ok_id, ok_raw) = blob(b"hello world");
    let (restricted_id, restricted_raw) = blob(b"secret stuff");
    let (denied_id, denied_raw) = blob(b"denied to alice");
    let (redacted_id, redacted_raw) = blob(b"redacted content");
    let (missing_id, _) = blob(b"never inserted");
    up.insert_object(ok_id, ok_raw.clone(), false);
    up.insert_object(restricted_id, restricted_raw.clone(), true);
    up.insert_object(denied_id, denied_raw, false);
    up.insert_object(redacted_id, redacted_raw, false);
    up.deny("github:alice", denied_id);
    up.redact(redacted_id);

    let alice = client(up, "alice").await;
    let ids = [ok_id, missing_id, denied_id, redacted_id, restricted_id];
    let (items, err) = fetch(&alice, &ids, None, None).await;
    assert!(err.is_none(), "{err:?}");
    let got: Vec<(ObjectId, i32)> = items.iter().map(|(id, it)| (*id, it.status)).collect();
    assert_eq!(
        got,
        vec![
            (ok_id, v1::ItemStatus::Ok as i32),
            (missing_id, v1::ItemStatus::NotFound as i32),
            (denied_id, v1::ItemStatus::Denied as i32),
            (redacted_id, v1::ItemStatus::Redacted as i32),
            (restricted_id, v1::ItemStatus::Ok as i32),
        ]
    );
    for (id, it) in &items {
        assert_eq!(it.finals, 1, "exactly one final chunk for {id}");
        if it.status != v1::ItemStatus::Ok as i32 {
            assert!(it.data.is_empty());
        }
    }
    let ok = &items[0].1;
    assert_eq!(verified(&ok_id, ok).unwrap(), ok_raw.to_vec());
    assert_eq!(ok.access, v1::AccessClass::Root as i32);
    assert_eq!(ok.total, ok_raw.len() as u64);
    assert_eq!(items[4].1.access, v1::AccessClass::Restricted as i32);

    // The same denied object is fine for bob.
    let bob = client(up, "bob").await;
    let (items, _) = fetch(&bob, &[denied_id], None, None).await;
    assert_eq!(items[0].1.status, v1::ItemStatus::Ok as i32);

    // HasObjects follows the same visibility.
    let present = alice
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
        })
        .await
        .unwrap()
        .into_inner()
        .present;
    assert_eq!(present, vec![true, false, false, false, true]);

    // zstd frames decompress to verified content; multi-chunk items reassemble.
    up.set_zstd(true);
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let (big_id, big_raw) = blob(&big);
    up.insert_object(big_id, big_raw, false);
    up.set_chunk_size(4096);
    let (items, err) = fetch(&alice, &[big_id, ok_id], None, None).await;
    assert!(err.is_none());
    for (id, it) in &items {
        assert_eq!(it.encoding, v1::Encoding::Zstd as i32);
        assert!(verified(id, it).is_some(), "{id}");
    }
    assert_eq!(items[0].1.total, 200_000);
    up.set_zstd(false);
    let (items, _) = fetch(&alice, &[big_id], None, None).await;
    assert_eq!(items[0].1.encoding, v1::Encoding::Raw as i32);
    assert_eq!(items[0].1.chunks, 200_000usize.div_ceil(4096));
    assert_eq!(verified(&big_id, &items[0].1).unwrap(), big);

    // Corruption: served bytes no longer match the id (raw and zstd).
    up.corrupt(ok_id);
    for z in [false, true] {
        up.set_zstd(z);
        let (items, _) = fetch(&alice, &[ok_id], None, None).await;
        assert_eq!(items[0].1.status, v1::ItemStatus::Ok as i32);
        assert!(verified(&ok_id, &items[0].1).is_none());
    }
    up.set_zstd(false);

    // Unauthenticated callers are rejected.
    let anon = alice.with_token(None);
    let st = anon
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![ok_id.as_bytes().to_vec()],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::Unauthenticated);
    let st = alice
        .with_token(Some("bogus".into()))
        .objects()
        .has_objects(v1::HasObjectsRequest { ids: vec![] })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::Unauthenticated);

    // GetServerInfo needs no token, publishes keys and epochs, and carries trailers.
    up.set_epochs(41, 42);
    let info = anon
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        scion_proto::convert::public_key_from_proto(&info.token_public_keys[0]).unwrap(),
        ("k1".to_owned(), [7; 32])
    );
    assert_eq!((info.policy_epoch, info.redaction_epoch), (41, 42));
    assert_eq!(
        anon.last_epochs(),
        Some(Epochs {
            policy: 41,
            redaction: 42,
            tier: Tier::Server
        })
    );

    // Malformed ids are INVALID_ARGUMENT.
    let st = alice
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![vec![1, 2, 3]],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_mixed_statuses_unix() {
    let up = FakeUpstream::start(opts(false)).await;
    assert!(matches!(up.endpoint(), scion_proto::Endpoint::Unix(_)));
    assert!(up.tls().is_none());
    batch_suite(&up).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_mixed_statuses_tls() {
    let up = FakeUpstream::start(opts(true)).await;
    assert!(matches!(up.endpoint(), scion_proto::Endpoint::Https(_)));
    assert!(up.local_addr().is_some());
    batch_suite(&up).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn range_reads_are_raw() {
    let up = FakeUpstream::start(opts(false)).await;
    let content: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 256) as u8).collect();
    let (id, raw) = blob(&content);
    let (other, other_raw) = blob(b"short");
    up.insert_object(id, raw, false);
    up.insert_object(other, other_raw, false);
    up.set_zstd(true);
    up.set_chunk_size(64);
    let c = client(&up, "alice").await;

    let (items, err) = fetch(&c, &[id], Some(100), Some(250)).await;
    assert!(err.is_none());
    let it = &items[0].1;
    assert_eq!(it.encoding, v1::Encoding::Raw as i32);
    assert_eq!(it.data, content[100..350].to_vec());
    assert_eq!(it.total, 1000);
    assert_eq!(it.finals, 1);

    // Offset only: to the end. Length past the end is clamped. Offset past the end: empty.
    let (items, _) = fetch(&c, &[id], Some(900), None).await;
    assert_eq!(items[0].1.data, content[900..].to_vec());
    let (items, _) = fetch(&c, &[id], None, Some(10)).await;
    assert_eq!(items[0].1.data, content[..10].to_vec());
    let (items, _) = fetch(&c, &[id, other], Some(995), Some(100)).await;
    assert_eq!(items[0].1.data, content[995..].to_vec());
    assert!(items[1].1.data.is_empty());
    assert_eq!(items[1].1.finals, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_kill_and_resume_by_offset() {
    let up = FakeUpstream::start(opts(false)).await;
    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
    let (id, raw) = blob(&content);
    up.insert_object(id, raw, false);
    up.set_chunk_size(16 * 1024);
    up.kill_stream_after_bytes(Some(40_000));
    let c = client(&up, "alice").await;

    // First attempt dies after exactly 40000 bytes with a retryable error.
    let (items, err) = fetch(&c, &[id], None, None).await;
    let st = err.expect("stream must be killed");
    let e = error_from_status(&st);
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert!(e.retryable());
    let mut got = items[0].1.data.clone();
    assert_eq!(got.len(), 40_000);
    assert_eq!(items[0].1.finals, 0);

    // Resume by offset until complete (the kill applies per stream).
    let mut attempts = 1;
    loop {
        attempts += 1;
        let (items, err) = fetch(&c, &[id], Some(got.len() as u64), None).await;
        got.extend_from_slice(&items[0].1.data);
        if err.is_none() {
            assert_eq!(items[0].1.finals, 1);
            break;
        }
        assert!(attempts < 10);
    }
    assert_eq!(attempts, 3);
    assert_eq!(got, content);
    assert_eq!(ObjectId::hash_blob(&got), id);

    let gets = up.requests_for("/scion.v1.ObjectService/GetObjects");
    assert_eq!(gets.len(), 3);
    assert!(gets.iter().all(|r| r.ids == vec![id]));

    // Disabling the knob restores full streams.
    up.kill_stream_after_bytes(None);
    let (items, err) = fetch(&c, &[id], None, None).await;
    assert!(err.is_none());
    assert_eq!(items[0].1.data, content);
}

#[tokio::test(flavor = "multi_thread")]
async fn down_fails_everything_unavailable() {
    let up = FakeUpstream::start(opts(true)).await;
    let (id, raw) = blob(b"x");
    up.insert_object(id, raw, false);
    let c = client(&up, "alice").await;
    up.set_down(true);
    up.set_epochs(5, 6);
    let st = c
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::Unavailable);
    // Error responses carry the trailers too.
    assert_eq!(
        c.last_epochs().map(|e| (e.policy, e.redaction)),
        Some((5, 6))
    );
    let st = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![id.as_bytes().to_vec()],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::Unavailable);
    let st = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::Unavailable);
    up.set_down(false);
    let (items, _) = fetch(&c, &[id], None, None).await;
    assert_eq!(items[0].1.status, v1::ItemStatus::Ok as i32);
}
