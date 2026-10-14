//! SPEC 7.8 / PLAN 6.13: `RawCodec` forwards unary and streaming calls byte-identically through
//! a test proxy (the daemon's verbatim forwarding mechanism).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod proto_support;

use bytes::Bytes;
use futures::future::BoxFuture;
use prost::Message;
use proto_support::{blob_id, start_unix};
use scion_proto::raw::RawCodec;
use scion_proto::{ScionClient, error_from_status, v1};
use std::convert::Infallible;
use std::task::{Context, Poll};
use tonic::server::NamedService;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};

fn path(p: &str) -> http::uri::PathAndQuery {
    http::uri::PathAndQuery::try_from(p.to_owned()).unwrap()
}

#[derive(Clone)]
struct UnaryFwd {
    ch: Channel,
    path: String,
}

impl tonic::server::UnaryService<Bytes> for UnaryFwd {
    type Response = Bytes;
    type Future = BoxFuture<'static, Result<Response<Bytes>, Status>>;

    fn call(&mut self, req: Request<Bytes>) -> Self::Future {
        let (ch, p) = (self.ch.clone(), self.path.clone());
        Box::pin(async move {
            let mut g = tonic::client::Grpc::new(ch);
            g.ready()
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            g.unary(req, path(&p), RawCodec).await
        })
    }
}

#[derive(Clone)]
struct StreamFwd {
    ch: Channel,
    path: String,
}

impl tonic::server::ServerStreamingService<Bytes> for StreamFwd {
    type Response = Bytes;
    type ResponseStream = Streaming<Bytes>;
    type Future = BoxFuture<'static, Result<Response<Streaming<Bytes>>, Status>>;

    fn call(&mut self, req: Request<Bytes>) -> Self::Future {
        let (ch, p) = (self.ch.clone(), self.path.clone());
        Box::pin(async move {
            let mut g = tonic::client::Grpc::new(ch);
            g.ready()
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            g.server_streaming(req, path(&p), RawCodec).await
        })
    }
}

/// Forwards every ObjectService method to `upstream` without decoding messages.
#[derive(Clone)]
struct Proxy {
    upstream: Channel,
}

impl NamedService for Proxy {
    const NAME: &'static str = "scion.v1.ObjectService";
}

impl tower_service::Service<http::Request<tonic::body::Body>> for Proxy {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<tonic::body::Body>) -> Self::Future {
        let ch = self.upstream.clone();
        Box::pin(async move {
            let p = req.uri().path().to_owned();
            let mut grpc = tonic::server::Grpc::new(RawCodec);
            let resp = if p.ends_with("/GetObjects") {
                grpc.server_streaming(StreamFwd { ch, path: p }, req).await
            } else {
                grpc.unary(UnaryFwd { ch, path: p }, req).await
            };
            Ok(resp)
        })
    }
}

async fn raw_unary(ch: Channel, p: &str, msg: Bytes) -> Result<Bytes, Status> {
    let mut g = tonic::client::Grpc::new(ch);
    g.ready().await.unwrap();
    Ok(g.unary(Request::new(msg), path(p), RawCodec)
        .await?
        .into_inner())
}

async fn raw_stream(ch: Channel, p: &str, msg: Bytes) -> Vec<Bytes> {
    let mut g = tonic::client::Grpc::new(ch);
    g.ready().await.unwrap();
    let mut s = g
        .server_streaming(Request::new(msg), path(p), RawCodec)
        .await
        .unwrap()
        .into_inner();
    let mut out = Vec::new();
    while let Some(m) = s.message().await.unwrap() {
        out.push(m);
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_codec_forwards_byte_identically() {
    let upstream = start_unix().await;
    let up_client = ScionClient::connect(&upstream.endpoint, None, None)
        .await
        .unwrap();

    // Proxy on its own Unix socket, forwarding to the upstream channel.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("proxy.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let proxy = Proxy {
        upstream: up_client.channel(),
    };
    let task = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(proxy)
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    let via = ScionClient::connect(&scion_proto::Endpoint::Unix(sock), None, None)
        .await
        .unwrap();

    // Unary, typed: identical messages.
    let direct = up_client
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    let proxied = via
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(direct, proxied);

    // Unary, raw bytes: identical encodings, including unknown fields in the request (a raw
    // request with an extra field 100 is forwarded as is and still decodes upstream).
    let mut req = v1::ResolveRefRequest {
        name: "refs/x".into(),
        at_version: Some(3),
    }
    .encode_to_vec();
    req.extend_from_slice(&[0xa0, 0x06, 0x2a]); // field 100, varint 42
    let p = "/scion.v1.ObjectService/ResolveRef";
    let a = raw_unary(up_client.channel(), p, Bytes::from(req.clone()))
        .await
        .unwrap();
    let b = raw_unary(via.channel(), p, Bytes::from(req)).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(v1::ResolveRefResponse::decode(a).unwrap().version, 6);

    // Unary error passes through with its ErrorDetail intact.
    let p_has = "/scion.v1.ObjectService/HasObjects";
    let msg = Bytes::from(
        v1::HasObjectsRequest {
            ids: vec![blob_id(1)],
        }
        .encode_to_vec(),
    );
    let ea = raw_unary(up_client.channel(), p_has, msg.clone())
        .await
        .unwrap_err();
    let eb = raw_unary(via.channel(), p_has, msg).await.unwrap_err();
    assert_eq!(ea.code(), eb.code());
    assert_eq!(ea.details(), eb.details());
    assert_eq!(error_from_status(&ea), error_from_status(&eb));

    // Server streaming: every message byte-identical, in order.
    let req = Bytes::from(
        v1::GetObjectsRequest {
            ids: vec![blob_id(1), blob_id(2), blob_id(3)],
            offset: Some(5000),
            length: None,
        }
        .encode_to_vec(),
    );
    let p = "/scion.v1.ObjectService/GetObjects";
    let a = raw_stream(up_client.channel(), p, req.clone()).await;
    let b = raw_stream(via.channel(), p, req).await;
    assert_eq!(a.len(), 3);
    assert_eq!(a, b);
    assert_eq!(
        a[0].len(),
        v1::ObjectChunk::decode(a[0].clone()).unwrap().encoded_len()
    );

    // Server streaming, typed through the proxy.
    let mut s = via
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![blob_id(9)],
            offset: None,
            length: None,
        })
        .await
        .unwrap()
        .into_inner();
    let c = s.message().await.unwrap().unwrap();
    assert_eq!(c.data, blob_id(9));
    assert!(s.message().await.unwrap().is_none());

    task.abort();
}
