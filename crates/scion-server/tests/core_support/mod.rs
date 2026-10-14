//! Shared helpers of the P23 core server tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::Principal;
use scion_derived::Registry;
use scion_proto::{Epochs, read_epochs};
use scion_server::testing::ServerTestOpts;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Options with a registry that has no eager types: the `history` and `git_mapping` types are
/// implemented by another package in this wave, so core tests do not run them.
pub fn opts() -> ServerTestOpts {
    ServerTestOpts {
        registry: Some(Arc::new(Registry::empty)),
        ..ServerTestOpts::default()
    }
}

pub fn alice() -> Principal {
    Principal::GitHub {
        login: "alice".into(),
        id: Some(1001),
    }
}

pub fn admin() -> Principal {
    Principal::GitHub {
        login: "admin".into(),
        id: Some(1),
    }
}

/// The epochs carried by a response's metadata (trailers merged into it by tonic, or the
/// trailers-only headers of an error status).
pub fn epochs(md: &tonic::metadata::MetadataMap) -> Option<Epochs> {
    read_epochs(&md.clone().into_headers())
}

/// Poll `f` until it holds (at most 10 s).
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// One plain HTTP/1.1 GET against the admin listener: `(status, body)`.
pub async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

/// Minimal `google.protobuf.FileDescriptorSet` decoding (only the fields the RPC enumeration
/// needs; prost skips the rest).
pub mod descriptor {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FileDescriptorSet {
        #[prost(message, repeated, tag = "1")]
        pub file: Vec<FileDescriptorProto>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FileDescriptorProto {
        #[prost(string, optional, tag = "2")]
        pub package: Option<String>,
        #[prost(message, repeated, tag = "6")]
        pub service: Vec<ServiceDescriptorProto>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ServiceDescriptorProto {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(message, repeated, tag = "2")]
        pub method: Vec<MethodDescriptorProto>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct MethodDescriptorProto {
        #[prost(string, optional, tag = "1")]
        pub name: Option<String>,
        #[prost(bool, optional, tag = "5")]
        pub client_streaming: Option<bool>,
        #[prost(bool, optional, tag = "6")]
        pub server_streaming: Option<bool>,
    }

    /// One RPC of `scion.v1`.
    #[derive(Clone, Debug)]
    pub struct Rpc {
        pub path: String,
        pub client_streaming: bool,
        pub server_streaming: bool,
    }

    /// Every RPC in `scion_proto::v1::FILE_DESCRIPTOR_SET`.
    pub fn rpcs() -> Vec<Rpc> {
        use prost::Message;
        let fds = FileDescriptorSet::decode(scion_proto::v1::FILE_DESCRIPTOR_SET).unwrap();
        let mut out = Vec::new();
        for f in fds
            .file
            .iter()
            .filter(|f| f.package.as_deref() == Some("scion.v1"))
        {
            for s in &f.service {
                for m in &s.method {
                    out.push(Rpc {
                        path: format!(
                            "/scion.v1.{}/{}",
                            s.name.as_deref().unwrap(),
                            m.name.as_deref().unwrap()
                        ),
                        client_streaming: m.client_streaming.unwrap_or(false),
                        server_streaming: m.server_streaming.unwrap_or(false),
                    });
                }
            }
        }
        out
    }
}

/// Call `rpc` with an empty request message through `channel` (raw bytes), returning the
/// status (OK when the call succeeded) and the response metadata (trailers included).
pub async fn raw_call(
    channel: tonic::transport::Channel,
    rpc: &descriptor::Rpc,
    token: Option<&str>,
) -> (tonic::Status, tonic::metadata::MetadataMap) {
    use scion_proto::raw::RawCodec;
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let path: http::uri::PathAndQuery = rpc.path.parse().unwrap();
    fn add_token<T>(mut r: tonic::Request<T>, token: Option<&str>) -> tonic::Request<T> {
        if let Some(t) = token {
            r.metadata_mut()
                .insert("authorization", format!("Bearer {t}").parse().unwrap());
        }
        r
    }
    let with_token = |r: tonic::Request<bytes::Bytes>| add_token(r, token);
    let ok = |md: tonic::metadata::MetadataMap| (tonic::Status::new(tonic::Code::Ok, ""), md);
    match (rpc.client_streaming, rpc.server_streaming) {
        (false, false) => {
            match grpc
                .unary(
                    with_token(tonic::Request::new(bytes::Bytes::new())),
                    path,
                    RawCodec,
                )
                .await
            {
                Ok(r) => ok(r.metadata().clone()),
                Err(s) => {
                    let md = s.metadata().clone();
                    (s, md)
                }
            }
        }
        (false, true) => {
            match grpc
                .server_streaming(
                    with_token(tonic::Request::new(bytes::Bytes::new())),
                    path,
                    RawCodec,
                )
                .await
            {
                Ok(r) => {
                    let mut stream = r.into_inner();
                    loop {
                        match stream.message().await {
                            Ok(Some(_)) => continue,
                            Ok(None) => {
                                let md = stream.trailers().await.unwrap().unwrap_or_default();
                                return ok(md);
                            }
                            Err(s) => {
                                let md = s.metadata().clone();
                                return (s, md);
                            }
                        }
                    }
                }
                Err(s) => {
                    let md = s.metadata().clone();
                    (s, md)
                }
            }
        }
        (true, _) => {
            let req = add_token(
                tonic::Request::new(futures::stream::empty::<bytes::Bytes>()),
                token,
            );
            match grpc.client_streaming(req, path, RawCodec).await {
                Ok(r) => ok(r.metadata().clone()),
                Err(s) => {
                    let md = s.metadata().clone();
                    (s, md)
                }
            }
        }
    }
}
