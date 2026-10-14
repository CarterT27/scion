//! SPEC 19.8 basics against FakeUpstream: per-item statuses and decompressed payloads are
//! identical through the daemon and direct; non-object RPCs are forwarded byte for byte with
//! the server's trailers; only the configured upstream is contacted. Also SPEC 7.1 trailers
//! with tier=daemon on daemon-served responses and the SPEC 15.2 daemon metrics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod daemon_support;

use bytes::Bytes;
use daemon_support::*;
use prost::Message;
use scion_core::{ObjectId, Tier};
use scion_daemon::UpstreamConnector;
use scion_proto::raw::RawCodec;
use scion_proto::{ScionClient, v1};
use scion_testkit::upstream_methods;
use std::time::Duration;

fn mixed(h: &Harness) -> (Vec<ObjectId>, ObjectId) {
    let (root, _) = h.blob(1, 150_000, false);
    let (restricted, _) = h.blob(2, 70_000, true);
    let (empty, _) = h.blob(3, 0, false);
    let (denied, _) = h.blob(4, 500, false);
    let (redacted, _) = h.blob(5, 500, false);
    h.up.deny(ALICE, denied);
    h.up.redact(redacted);
    let missing = missing_id(9);
    (
        vec![root, missing, restricted, denied, redacted, empty, root],
        redacted,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn mixed_batch_is_identical_through_daemon_and_direct() {
    for zstd in [false, true] {
        let h = Harness::start(Opts::default()).await;
        h.up.set_zstd(zstd);
        h.up.set_chunk_size(16 * 1024);
        let (ids, _) = mixed(&h);
        let direct = h.direct(ALICE).await;
        let via = h.via_daemon(ALICE).await;
        let want = get_objects(&direct, &ids).await.unwrap();
        assert_eq!(
            want.iter().map(|i| i.status).collect::<Vec<_>>(),
            vec![OK, NOT_FOUND, OK, DENIED, REDACTED, OK, OK]
        );
        // Cold (fetched upstream), then warm (served from cache), then warm again.
        for round in 0..3 {
            let got = get_objects(&via, &ids).await.unwrap();
            let strip = |v: &[Item]| {
                v.iter()
                    .map(|i| {
                        (
                            i.id,
                            i.status,
                            i.access_class,
                            i.total_size,
                            i.content.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(strip(&got), strip(&want), "zstd={zstd} round={round}");
        }
        let stats = h.daemon().stats();
        assert!(stats.cache_hits >= 6, "{stats:?}");
        // Range reads serve raw bytes with identical offsets and content.
        for (off, len) in [
            (Some(10), Some(100)),
            (Some(149_990), None),
            (None, Some(5)),
        ] {
            let a = normalize(get_objects_raw(&direct, &ids[..1], off, len).await.unwrap());
            let b = normalize(get_objects_raw(&via, &ids[..1], off, len).await.unwrap());
            assert_eq!(a, b, "range {off:?} {len:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn has_objects_and_server_info_match_direct() {
    let h = Harness::start(Opts::default()).await;
    let (ids, _) = mixed(&h);
    let direct = h.direct(ALICE).await;
    let via = h.via_daemon(ALICE).await;
    let req = v1::HasObjectsRequest {
        ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
    };
    let a = direct
        .objects()
        .has_objects(req.clone())
        .await
        .unwrap()
        .into_inner();
    let b = via.objects().has_objects(req).await.unwrap().into_inner();
    assert_eq!(a, b);
    // HasObjects is forwarded: the upstream saw the daemon's call with the caller identity.
    let has = h.up.requests_for(upstream_methods::HAS_OBJECTS);
    assert_eq!(has.len(), 2);
    assert_eq!(has[1].principal.as_deref(), Some(ALICE));
    let a = direct
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    let b = via
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a, b);
}

async fn raw_unary(
    c: &ScionClient,
    path: &str,
    msg: Bytes,
) -> Result<(Bytes, Option<scion_proto::Epochs>), tonic::Status> {
    let mut g = tonic::client::Grpc::new(c.channel());
    g.ready().await.unwrap();
    let mut req = tonic::Request::new(msg);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", c.token().unwrap()).parse().unwrap(),
    );
    let resp = g
        .unary(
            req,
            http::uri::PathAndQuery::try_from(path.to_owned()).unwrap(),
            RawCodec,
        )
        .await?;
    let epochs = scion_proto::read_epochs(&resp.metadata().clone().into_headers());
    Ok((resp.into_inner(), epochs))
}

#[tokio::test(flavor = "multi_thread")]
async fn query_and_auth_calls_are_forwarded_byte_identically() {
    let h = Harness::start(Opts::default()).await;
    let cs = ObjectId::hash_encoded(scion_core::Kind::Changeset, b"changeset");
    h.up.insert_changeset(cs, Bytes::from_static(b"\x01encoded-changeset"));
    h.up.set_ref("main", cs, 1);
    let (blob, _) = h.blob(1, 10, false);
    h.up.set_epochs(7, 0);
    let direct = h.direct(ALICE).await;
    let via = h.via_daemon(ALICE).await;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            upstream_methods::GET_CHANGESETS,
            v1::GetChangesetsRequest {
                ids: vec![cs.as_bytes().to_vec(), missing_id(3).as_bytes().to_vec()],
            }
            .encode_to_vec(),
        ),
        (
            upstream_methods::RESOLVE,
            v1::ResolveRequest {
                revision: "main".into(),
            }
            .encode_to_vec(),
        ),
        (
            upstream_methods::WHO_AM_I,
            v1::WhoAmIRequest {}.encode_to_vec(),
        ),
        (
            upstream_methods::CHECK_ACCESS,
            v1::CheckAccessRequest {
                subject_token: h.token(BOB),
                object_ids: vec![blob.as_bytes().to_vec()],
            }
            .encode_to_vec(),
        ),
    ];
    for (path, mut body) in cases {
        // An unknown field must survive forwarding untouched.
        body.extend_from_slice(&[0xa0, 0x06, 0x2a]);
        let (a, ea) = raw_unary(&direct, path, Bytes::from(body.clone()))
            .await
            .unwrap();
        let (b, eb) = raw_unary(&via, path, Bytes::from(body)).await.unwrap();
        assert_eq!(a, b, "{path}");
        assert_eq!(ea, eb, "{path}: trailers forwarded unmodified");
        assert_eq!(eb.unwrap().tier, Tier::Server, "{path}");
    }
    // Errors pass through with their ErrorDetail.
    let bad = v1::ResolveRequest {
        revision: "nope".into(),
    }
    .encode_to_vec();
    let ea = raw_unary(&direct, upstream_methods::RESOLVE, Bytes::from(bad.clone()))
        .await
        .unwrap_err();
    let eb = raw_unary(&via, upstream_methods::RESOLVE, Bytes::from(bad))
        .await
        .unwrap_err();
    assert_eq!(ea.code(), eb.code());
    assert_eq!(ea.details(), eb.details());
    // Unimplemented upstream methods are forwarded too (no local interpretation).
    let e = via
        .query()
        .log(v1::LogRequest::default())
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unimplemented);
    // The forwarded trailers taught the daemon the policy epoch.
    assert_eq!(h.daemon().stats().policy_epoch, 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_served_responses_carry_tier_daemon_trailers() {
    let h = Harness::start(Opts::default()).await;
    h.up.set_epochs(3, 0);
    let (id, _) = h.blob(1, 100, false);
    let via = h.via_daemon(ALICE).await;
    get_one(&via, id).await.unwrap();
    let e = via.last_epochs().unwrap();
    assert_eq!(e.tier, Tier::Daemon);
    assert_eq!(e.policy, 3);
    via.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    assert_eq!(via.last_epochs().unwrap().tier, Tier::Daemon);
    h.up.set_ref("main", id, 1);
    via.objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap();
    assert_eq!(via.last_epochs().unwrap().tier, Tier::Daemon);
    // Errors served by the daemon carry them too (trailers-only).
    let anon = ScionClient::connect(&h.daemon().endpoint(), None, None)
        .await
        .unwrap();
    let err = anon
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![id.as_bytes().to_vec()],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert_eq!(anon.last_epochs().unwrap().tier, Tier::Daemon);
    // A forwarded call keeps the server's tier.
    via.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(via.last_epochs().unwrap().tier, Tier::Server);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_configured_upstream_is_contacted() {
    let h = Harness::start(Opts::default()).await;
    let (id, _) = h.blob(1, 100, false);
    let via = h.via_daemon(ALICE).await;
    get_one(&via, id).await.unwrap();
    via.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let _ = get_one(&via, missing_id(1)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.conn.destinations(), vec![h.cfg.upstream.clone()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_is_world_accessible() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start(Opts::default()).await;
    let mode = std::fs::metadata(h.daemon().socket_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o666);
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_metrics_move() {
    let h = Harness::start(Opts::default()).await;
    let (id, _) = h.blob(1, 1000, false);
    let via = h.via_daemon(ALICE).await;
    get_one(&via, id).await.unwrap();
    get_one(&via, id).await.unwrap();
    h.up.redact(missing_id(7));
    via.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert!(
        eventually(Duration::from_secs(5), || h
            .daemon()
            .stats()
            .redaction_epoch
            == 1)
        .await
    );
    let text = h.metrics.render();
    let d = "tier=\"daemon\"";
    assert!(metric_sum(&text, "scion_cache_requests_total", "result=\"hit\"") >= 1.0);
    assert!(metric_sum(&text, "scion_cache_requests_total", "result=\"miss\"") >= 1.0);
    assert!(metric_sum(&text, "scion_cache_bytes", d) > 0.0);
    assert!(metric_sum(&text, "scion_upstream_requests_total", d) >= 2.0);
    assert_eq!(metric_sum(&text, "scion_redaction_epoch", d), 1.0);
    assert!(text.contains("scion_redaction_lag_seconds{tier=\"daemon\"}"));
    assert!(metric_sum(&text, "scion_rpc_duration_seconds_count", d) >= 3.0);
    assert!(metric_sum(&text, "scion_object_bytes_served_total", d) >= 2000.0);
    let s = h.daemon().stats();
    assert!(s.upstream_requests >= 2 && s.cache_hits >= 1 && s.cache_misses >= 1);
}
