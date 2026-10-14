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
            (Some(200_000), Some(10)),
            (Some(0), Some(0)),
        ] {
            let a = normalize(get_objects_raw(&direct, &ids[..1], off, len).await.unwrap());
            let b = normalize(get_objects_raw(&via, &ids[..1], off, len).await.unwrap());
            assert_eq!(a, b, "range {off:?} {len:?}");
            // The same range over the whole mixed batch: status items unchanged.
            let a = normalize(get_objects_raw(&direct, &ids, off, len).await.unwrap());
            let b = normalize(get_objects_raw(&via, &ids, off, len).await.unwrap());
            assert_eq!(a, b, "batch range {off:?} {len:?}");
        }
        // A range read of an object the daemon has not cached yet.
        let (fresh, _) = h.blob(77, 40_000, false);
        let a = normalize(
            get_objects_raw(&direct, &[fresh], Some(30_000), Some(20_000))
                .await
                .unwrap(),
        );
        let b = normalize(
            get_objects_raw(&via, &[fresh], Some(30_000), Some(20_000))
                .await
                .unwrap(),
        );
        assert_eq!(a, b, "cold range read");
        assert_eq!(b[0].content.len(), 10_000);
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
        // The metric is published right after the set is applied: wait for the metric.
        eventually(Duration::from_secs(5), || metric_sum(
            &h.metrics.render(),
            "scion_redaction_epoch",
            "tier=\"daemon\""
        ) == 1.0)
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

/// SPEC 17: TLS is required between daemon and server. Local and forwarded paths work over a
/// TLS upstream exactly as over plaintext: identical items, forwarded trailers intact.
#[tokio::test(flavor = "multi_thread")]
async fn works_over_a_tls_upstream() {
    let h = Harness::start(Opts {
        tls: true,
        ..Default::default()
    })
    .await;
    assert!(h.up.tls().is_some(), "upstream serves TLS");
    assert!(h.cfg.upstream.starts_with("https://"), "{}", h.cfg.upstream);
    h.up.set_epochs(4, 0);
    let (ids, _) = mixed(&h);
    let direct = h.direct(ALICE).await;
    let via = h.via_daemon(ALICE).await;
    let want = get_objects(&direct, &ids).await.unwrap();
    for _ in 0..2 {
        assert_eq!(get_objects(&via, &ids).await.unwrap(), want);
        assert_eq!(via.last_epochs().unwrap().tier, Tier::Daemon);
    }
    // Forwarded calls (unary, plus HasObjects) cross the TLS hop with the server's trailers.
    let a = direct
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .unwrap()
        .into_inner();
    let b = via
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a, b);
    let e = via.last_epochs().unwrap();
    assert_eq!((e.tier, e.policy), (Tier::Server, 4));
    let req = v1::HasObjectsRequest {
        ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
    };
    assert_eq!(
        direct
            .objects()
            .has_objects(req.clone())
            .await
            .unwrap()
            .into_inner(),
        via.objects().has_objects(req).await.unwrap().into_inner()
    );
    assert_eq!(h.conn.destinations(), vec![h.cfg.upstream.clone()]);
}

/// A burst of first requests on a fresh daemon loads the token keys with a single
/// GetServerInfo call (plus at most the poll loop's own refresh), not one per request.
#[tokio::test(flavor = "multi_thread")]
async fn first_request_burst_loads_keys_once() {
    let h = Harness::start(Opts {
        tweak: Box::new(|c| c.redaction_poll_interval_ms = 600_000),
        ..Default::default()
    })
    .await;
    let (id, data) = h.blob(1, 100, false);
    let via = h.via_daemon(ALICE).await;
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let c = via.clone();
        tasks.push(tokio::spawn(async move { get_one(&c, id).await }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().content, data.to_vec());
    }
    let n = h.upstream_calls(upstream_methods::GET_SERVER_INFO);
    assert!((1..=2).contains(&n), "GetServerInfo calls = {n}");
}

/// An upstream that echoes every gRPC request verbatim: the response body is the request's
/// message frames, and the trailers report the request headers it saw plus a custom trailer.
#[derive(Clone)]
struct EchoUpstream;

impl tower_service::Service<http::Request<tonic::body::Body>> for EchoUpstream {
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
        use http_body_util::BodyExt;
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let data = body.collect().await.unwrap().to_bytes();
            let mut t = http::HeaderMap::new();
            t.insert("grpc-status", "0".parse().unwrap());
            t.insert("x-echo-path", parts.uri.path().parse().unwrap());
            for h in ["authorization", "x-custom"] {
                if let Some(v) = parts.headers.get(h) {
                    t.insert(
                        http::HeaderName::from_bytes(format!("x-echo-{h}").as_bytes()).unwrap(),
                        v.clone(),
                    );
                }
            }
            t.insert("scion-tier", "server".parse().unwrap());
            t.insert("scion-policy-epoch", "11".parse().unwrap());
            t.insert("scion-redaction-epoch", "0".parse().unwrap());
            let frames: Vec<Result<http_body::Frame<Bytes>, tonic::Status>> = vec![
                Ok(http_body::Frame::data(data)),
                Ok(http_body::Frame::trailers(t)),
            ];
            let body = http_body_util::StreamBody::new(futures::stream::iter(frames));
            let resp = http::Response::builder()
                .header("content-type", "application/grpc")
                .body(tonic::body::Body::new(body))
                .unwrap();
            Ok(resp)
        })
    }
}

/// SPEC 7.8 for streaming RPCs: a client-streaming and bidirectional call through the daemon
/// reaches the upstream with every message frame and header (authorization, custom metadata)
/// unmodified, and the upstream's response frames and trailers come back unmodified.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_calls_are_forwarded_verbatim() {
    let up_dir = tempfile::tempdir().unwrap();
    let sock = up_dir.path().join("echo.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        let _ = tonic::transport::Server::builder()
            .serve_with_incoming(EchoUpstream, incoming)
            .await;
    });
    let dir = tempfile::tempdir().unwrap();
    let ep = scion_proto::Endpoint::Unix(sock);
    let cfg = scion_config::DaemonConfig::for_tests(dir.path(), &ep.to_string());
    let conn = std::sync::Arc::new(scion_daemon::DirectUpstream::new(ep, None));
    let d = scion_daemon::Daemon::start(
        cfg,
        conn,
        std::sync::Arc::new(scion_core::SystemClock),
        scion_obs::Metrics::new(),
    )
    .await
    .unwrap();
    let c = ScionClient::connect(&d.endpoint(), None, None)
        .await
        .unwrap();
    let msgs: Vec<Bytes> = (0..50u8)
        .map(|i| Bytes::from(vec![i; 1 + usize::from(i) * 997]))
        .collect();
    for path in [
        "/scion.v1.WriteService/UploadObjects",
        "/scion.v1.QueryService/Diff",
    ] {
        let mut g = tonic::client::Grpc::new(c.channel());
        g.ready().await.unwrap();
        let mut req = tonic::Request::new(futures::stream::iter(msgs.clone()));
        req.metadata_mut()
            .insert("authorization", "Bearer opaque-token".parse().unwrap());
        req.metadata_mut()
            .insert("x-custom", "kept-as-is".parse().unwrap());
        let resp = g
            .streaming(
                req,
                http::uri::PathAndQuery::try_from(path.to_owned()).unwrap(),
                RawCodec,
            )
            .await
            .unwrap();
        let mut s = resp.into_inner();
        let mut got = Vec::new();
        while let Some(m) = s.message().await.unwrap() {
            got.push(m);
        }
        assert_eq!(got, msgs, "{path}: frames unmodified");
        let t = s.trailers().await.unwrap().unwrap();
        assert_eq!(t.get("x-echo-path").unwrap().to_str().unwrap(), path);
        assert_eq!(
            t.get("x-echo-authorization").unwrap().to_str().unwrap(),
            "Bearer opaque-token"
        );
        assert_eq!(
            t.get("x-echo-x-custom").unwrap().to_str().unwrap(),
            "kept-as-is"
        );
        let e = scion_proto::read_epochs(&t.clone().into_headers()).unwrap();
        assert_eq!((e.tier, e.policy), (Tier::Server, 11));
    }
    assert_eq!(
        d.stats().policy_epoch,
        11,
        "epochs observed from forwarded trailers"
    );
    d.shutdown().await;
    server.abort();
}
