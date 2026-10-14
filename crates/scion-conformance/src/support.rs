//! Shared helpers for the conformance suites (PLAN 2.22, 3.6; P30).
//!
//! Integration tests cannot share modules with each other, and this crate's `lib.rs` only
//! re-exports the cluster types, so the helpers are associated items of [`TestCluster`]:
//! principals and the default policy, RPC conveniences (upload, commit, verified object
//! fetches), descriptor-driven RPC enumeration with raw calls, trailer decoding, and the
//! admin-listener / Prometheus text helpers used by the observability suite.
// SAFETY-ish: test-harness helpers; a failed setup step cannot be recovered by a suite, so
// they fail loudly instead of returning errors the suites would unwrap anyway.
#![allow(clippy::expect_used, clippy::panic)]

use crate::cluster::{TestCluster, Via};
use bytes::Bytes;
use scion_core::{FaultRegistry, Kind, ObjectId, Principal};
use scion_object::RefName;
use scion_proto::convert::{DELTA_OP_DELETE, DELTA_OP_PUT};
use scion_proto::{Epochs, ScionClient, read_epochs, v1};
use scion_store::MemHandles;
use std::net::SocketAddr;
use std::sync::Arc;
use tonic::metadata::MetadataMap;

/// One object of a `GetObjects` response, reassembled from its chunks (SPEC 7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedItem {
    /// The id.
    pub id: ObjectId,
    /// `v1::ItemStatus` as i32.
    pub status: i32,
    /// `v1::Encoding` as i32 (of the first chunk).
    pub encoding: i32,
    /// Decompressed size reported by the tier.
    pub total_size: u64,
    /// `v1::AccessClass` as i32.
    pub access_class: i32,
    /// Offset of the first chunk (range reads).
    pub offset: u64,
    /// Decompressed content (empty for non-ok items).
    pub content: Vec<u8>,
    /// True when the decompressed content of a whole-object read hashes to `id` (SPEC 7.3:
    /// the receiver verifies before use).
    pub verified: bool,
}

/// A complete `GetObjects` response (SPEC 7.3).
#[derive(Clone, Debug)]
pub struct Fetched {
    /// Items in response order.
    pub items: Vec<FetchedItem>,
    /// Response headers merged with the trailers.
    pub metadata: MetadataMap,
}

/// One RPC of `scion.v1`, from the file descriptor set (SPEC 19.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcInfo {
    /// Full gRPC path, e.g. `/scion.v1.ObjectService/GetObjects`.
    pub path: String,
    /// Service name, e.g. `ObjectService`.
    pub service: String,
    /// Method name, e.g. `GetObjects`.
    pub method: String,
    /// Client streaming.
    pub client_streaming: bool,
    /// Server streaming.
    pub server_streaming: bool,
}

/// Minimal `google.protobuf.FileDescriptorSet` decoding (only what RPC enumeration needs).
mod descriptor {
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
}

/// The id the content of a whole-object read must hash to (SPEC 4.2).
fn id_of(kind: Kind, content: &[u8]) -> ObjectId {
    match kind {
        Kind::Blob => ObjectId::hash_blob(content),
        k => ObjectId::hash_encoded(k, content),
    }
}

impl TestCluster {
    /// The default cluster policy (PLAN 2.22): groups `eng`, `admins` and `security`; `/` is
    /// readable and writable by `eng`; `/secrets` only by `security` and `admins`. `eng`
    /// updates every ref, `admins` may force and hold every `[admin]` permission.
    /// `github:alice`, `github:bob` and the services are `eng` (no `/secrets` access),
    /// `github:carol` is `security`, `github:admin` is `admins`, and `github:mallory` has no
    /// grant at all.
    pub const DEFAULT_ACL: &'static str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "service:ci", "service:agent", "service:importer", "group:security", "group:admins"]
security = ["github:carol"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security", "group:admins"]
write = ["group:security", "group:admins"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

    /// Service principals configured on every server by [`crate::ClusterOpts::new`]:
    /// `(name, plaintext secret)` (SPEC 12.3).
    pub const SERVICES: &'static [(&'static str, &'static str)] = &[
        ("ci", "ci-secret-7f3a9c51d2e84b06"),
        ("agent", "agent-secret-41d2e8a07c9f3b65"),
        ("importer", "importer-secret-9b6c05e1f4a2d873"),
    ];

    /// `github:<login>` without a numeric id.
    pub fn gh(login: &str) -> Principal {
        Principal::GitHub {
            login: login.to_owned(),
            id: None,
        }
    }

    /// The principal [`TestCluster::seed`] writes as (`github:admin`: write everywhere and
    /// `admin.refs`, so parentless commits may use `import_mode`).
    pub fn seeder() -> Principal {
        Self::gh("admin")
    }

    /// A client sending `token` (or none), directly to `servers[0]` or through the daemon.
    pub async fn client_with_token(&self, via: Via, token: Option<String>) -> ScionClient {
        match via {
            Via::Server => {
                let s = &self.servers[0];
                ScionClient::connect(&s.endpoint, s.tls.clone(), token)
                    .await
                    .expect("connect to servers[0]")
            }
            Via::Daemon => {
                self.daemon
                    .as_ref()
                    .expect("this cluster has no daemon")
                    .client(token)
                    .await
            }
        }
    }

    /// The in-memory backend handles (`None` on the External backend).
    pub fn mem(&self) -> Option<&MemHandles> {
        self.servers[0].mem.as_ref()
    }

    /// The shared storage fault registry (SPEC 19.0 fault hooks).
    pub fn faults(&self) -> Arc<FaultRegistry> {
        self.storage.faults.clone()
    }

    /// Current `(target, version)` of ref `name`, read from storage.
    pub async fn ref_head(&self, name: &str) -> (ObjectId, u64) {
        let r = self.servers[0]
            .ctx()
            .history
            .resolve_ref(&RefName::parse(name).expect("valid ref name"), None)
            .await
            .expect("resolve ref");
        (r.target, r.version)
    }

    /// Deterministic change id bytes from `n` (SPEC 4.3.5).
    pub fn change_id(n: u32) -> Vec<u8> {
        let mut b = vec![0xC5; 16];
        b[12..].copy_from_slice(&n.to_be_bytes());
        b
    }

    /// Meta fields with a fixed author and time (SPEC 4.3.3).
    pub fn meta(msg: &str) -> v1::MetaFields {
        v1::MetaFields {
            author: b"Conformance <conformance@example.com>".to_vec(),
            author_time: Some(v1::Timestamp {
                seconds: 1_700_000_000,
                tz_offset_minutes: 0,
            }),
            committer: None,
            committer_time: None,
            message: msg.as_bytes().to_vec(),
            extensions: Vec::new(),
        }
    }

    /// Upload raw blobs through `client` (`UploadObjects`, every item `ok`; SPEC 7.5).
    pub async fn upload(client: &ScionClient, blobs: &[&[u8]]) -> Vec<ObjectId> {
        let blobs: Vec<Bytes> = blobs.iter().map(|b| Bytes::copy_from_slice(b)).collect();
        crate::seed::upload_blobs(client, &blobs)
            .await
            .unwrap_or_else(|e| panic!("upload failed: {e}"))
    }

    /// The `CreateChangeset` request putting `puts` (path, content; blobs must exist) and
    /// deleting `deletes` on `parent` (SPEC 7.5, 8.1).
    pub fn create_request(
        parent: &ObjectId,
        puts: &[(&str, &[u8])],
        deletes: &[&str],
        change: u32,
    ) -> v1::CreateChangesetRequest {
        let mut delta: Vec<v1::DeltaEntryMsg> = puts
            .iter()
            .map(|(p, c)| v1::DeltaEntryMsg {
                path: p.as_bytes().to_vec(),
                op: DELTA_OP_PUT,
                mode: 1,
                id: ObjectId::hash_blob(c).as_bytes().to_vec(),
                size: c.len() as u64,
            })
            .chain(deletes.iter().map(|p| v1::DeltaEntryMsg {
                path: p.as_bytes().to_vec(),
                op: DELTA_OP_DELETE,
                mode: 0,
                id: Vec::new(),
                size: 0,
            }))
            .collect();
        delta.sort_by(|a, b| a.path.cmp(&b.path));
        v1::CreateChangesetRequest {
            parents: vec![parent.as_bytes().to_vec()],
            delta,
            meta_fields: Some(Self::meta(&format!("change {change}"))),
            change_id: Self::change_id(change),
            expected_root_tree: None,
            import_mode: false,
        }
    }

    /// Upload the contents of `puts`, then create a changeset on `parent` through `client`
    /// (SPEC 7.5). Panics on failure.
    pub async fn commit(
        client: &ScionClient,
        parent: &ObjectId,
        puts: &[(&str, &[u8])],
        deletes: &[&str],
        change: u32,
    ) -> ObjectId {
        let contents: Vec<&[u8]> = puts.iter().map(|(_, c)| *c).collect();
        Self::upload(client, &contents).await;
        let r = client
            .write()
            .create_changeset(Self::create_request(parent, puts, deletes, change))
            .await
            .unwrap_or_else(|s| panic!("CreateChangeset failed: {s:?}"))
            .into_inner();
        ObjectId::from_bytes(&r.changeset_id).expect("changeset id")
    }

    /// `GetObjects` through `client`, chunks reassembled per item, ZSTD payloads decompressed
    /// and whole-object reads verified against their ids (SPEC 7.3). Trailers are merged into
    /// the returned metadata. A failed call (or a stream ending with an error status) is the
    /// `Err` status.
    pub async fn fetch(
        client: &ScionClient,
        ids: &[ObjectId],
        offset: Option<u64>,
        length: Option<u64>,
    ) -> Result<Fetched, tonic::Status> {
        let req = v1::GetObjectsRequest {
            ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
            offset,
            length,
        };
        let resp = client.objects().get_objects(req).await?;
        let mut metadata = resp.metadata().clone();
        let mut stream = resp.into_inner();
        let mut items: Vec<FetchedItem> = Vec::new();
        let mut raw: Vec<Vec<u8>> = Vec::new();
        let mut open = false;
        while let Some(chunk) = stream.message().await? {
            if !open {
                let id = ObjectId::from_bytes(&chunk.id)
                    .map_err(|e| tonic::Status::internal(format!("bad chunk id: {e}")))?;
                items.push(FetchedItem {
                    id,
                    status: chunk.status,
                    encoding: chunk.encoding,
                    total_size: chunk.total_size,
                    access_class: chunk.access_class,
                    offset: chunk.offset,
                    content: Vec::new(),
                    verified: false,
                });
                raw.push(Vec::new());
            }
            if let Some(last) = raw.last_mut() {
                last.extend_from_slice(&chunk.data);
            }
            open = !chunk.r#final;
        }
        if let Some(t) = stream.trailers().await? {
            let mut h = metadata.clone().into_headers();
            h.extend(t.into_headers());
            metadata = MetadataMap::from_headers(h);
        }
        let ranged = offset.is_some() || length.is_some();
        for (item, data) in items.iter_mut().zip(raw) {
            if item.status != v1::ItemStatus::Ok as i32 {
                continue;
            }
            item.content = if item.encoding == v1::Encoding::Zstd as i32 {
                zstd::decode_all(&data[..])
                    .map_err(|e| tonic::Status::data_loss(format!("bad zstd frame: {e}")))?
            } else {
                data
            };
            item.verified = !ranged && id_of(item.id.kind(), &item.content) == item.id;
        }
        Ok(Fetched { items, metadata })
    }

    /// The `scion.v1.ErrorDetail` carried in a status's details (SPEC 7.2), if any.
    pub fn error_detail(s: &tonic::Status) -> Option<v1::ErrorDetail> {
        use prost::Message;
        if s.details().is_empty() {
            return None;
        }
        v1::ErrorDetail::decode(s.details()).ok()
    }

    /// The three SPEC 7.1 trailers decoded from response metadata (trailers merged by tonic
    /// for unary calls and statuses).
    pub fn epochs(md: &MetadataMap) -> Option<Epochs> {
        read_epochs(&md.clone().into_headers())
    }

    /// Every RPC in `scion_proto::v1::FILE_DESCRIPTOR_SET` (SPEC 19.7: the matrix is
    /// descriptor-driven so an RPC without a case fails).
    pub fn rpcs() -> Vec<RpcInfo> {
        use prost::Message;
        let fds = descriptor::FileDescriptorSet::decode(scion_proto::v1::FILE_DESCRIPTOR_SET)
            .expect("decode the scion.v1 descriptor set");
        let mut out = Vec::new();
        for f in fds
            .file
            .iter()
            .filter(|f| f.package.as_deref() == Some("scion.v1"))
        {
            for s in &f.service {
                let service = s.name.clone().unwrap_or_default();
                for m in &s.method {
                    let method = m.name.clone().unwrap_or_default();
                    out.push(RpcInfo {
                        path: format!("/scion.v1.{service}/{method}"),
                        service: service.clone(),
                        method,
                        client_streaming: m.client_streaming.unwrap_or(false),
                        server_streaming: m.server_streaming.unwrap_or(false),
                    });
                }
            }
        }
        out
    }

    /// Call `rpc` with the encoded request message `body` (for client-streaming RPCs: one
    /// stream item, or none when `body` is `None`) through `channel` with RawCodec, returning
    /// the final status (OK when the call succeeded) and the response metadata with trailers.
    pub async fn raw_call(
        channel: tonic::transport::Channel,
        rpc: &RpcInfo,
        body: Option<Bytes>,
        token: Option<&str>,
    ) -> (tonic::Status, MetadataMap) {
        use scion_proto::raw::RawCodec;
        let mut grpc = tonic::client::Grpc::new(channel);
        if let Err(e) = grpc.ready().await {
            return (
                tonic::Status::unavailable(format!("channel not ready: {e}")),
                MetadataMap::new(),
            );
        }
        let path: tonic::codegen::http::uri::PathAndQuery =
            rpc.path.parse().expect("valid gRPC path");
        fn add_token<T>(mut r: tonic::Request<T>, token: Option<&str>) -> tonic::Request<T> {
            if let Some(t) = token
                && let Ok(v) = format!("Bearer {t}").parse()
            {
                r.metadata_mut().insert("authorization", v);
            }
            r
        }
        fn failed(s: tonic::Status) -> (tonic::Status, MetadataMap) {
            let md = s.metadata().clone();
            (s, md)
        }
        let ok = |md: MetadataMap| (tonic::Status::new(tonic::Code::Ok, ""), md);
        if rpc.client_streaming {
            let items: Vec<Bytes> = body.into_iter().collect();
            let req = add_token(tonic::Request::new(futures::stream::iter(items)), token);
            return match grpc.client_streaming(req, path, RawCodec).await {
                Ok(r) => ok(r.metadata().clone()),
                Err(s) => failed(s),
            };
        }
        let req = add_token(tonic::Request::new(body.unwrap_or_default()), token);
        if !rpc.server_streaming {
            return match grpc.unary(req, path, RawCodec).await {
                Ok(r) => ok(r.metadata().clone()),
                Err(s) => failed(s),
            };
        }
        match grpc.server_streaming(req, path, RawCodec).await {
            Ok(r) => {
                let mut md = r.metadata().clone();
                let mut stream = r.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            if let Ok(Some(t)) = stream.trailers().await {
                                let mut h = md.into_headers();
                                h.extend(t.into_headers());
                                md = MetadataMap::from_headers(h);
                            }
                            return ok(md);
                        }
                        Err(s) => return failed(s),
                    }
                }
            }
            Err(s) => failed(s),
        }
    }

    /// One plain HTTP/1.1 GET against an admin listener (SPEC 15.3): `(status, body)`.
    pub async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the admin listener");
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send HTTP request");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.expect("read HTTP response");
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text
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

    /// Sum of the Prometheus text samples named exactly `name` whose labels include every
    /// `(label, value)` of `labels` (SPEC 15.2). `None` when no sample matches.
    pub fn metric(text: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let mut found = None;
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (head, value) = match line.rsplit_once(' ') {
                Some(v) => v,
                None => continue,
            };
            let (sample, label_text) = match head.split_once('{') {
                Some((n, rest)) => (n, rest.trim_end_matches('}')),
                None => (head, ""),
            };
            if sample != name {
                continue;
            }
            let all = labels
                .iter()
                .all(|(k, v)| label_text.contains(&format!("{k}=\"{v}\"")));
            if !all {
                continue;
            }
            if let Ok(v) = value.trim().parse::<f64>() {
                found = Some(found.unwrap_or(0.0) + v);
            }
        }
        found
    }

    /// The Prometheus text of `servers[i]`'s admin listener (`/metrics`; SPEC 15.2).
    pub async fn server_metrics(&self, i: usize) -> String {
        let addr = self.servers[i]
            .admin_addr()
            .expect("the server has an admin listener");
        let (code, body) = Self::http_get(addr, "/metrics").await;
        assert_eq!(code, 200, "/metrics status");
        body
    }

    /// The Prometheus text of the daemon's metrics registry (SPEC 15.2).
    pub fn daemon_metrics(&self) -> String {
        self.daemon
            .as_ref()
            .expect("this cluster has no daemon")
            .handle
            .metrics()
            .render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_parsing() {
        let text = "# HELP x y\n# TYPE x counter\n\
                    scion_a_total{tier=\"server\",kind=\"blob\"} 3\n\
                    scion_a_total{tier=\"daemon\",kind=\"blob\"} 4\n\
                    scion_b 2.5\n";
        assert_eq!(TestCluster::metric(text, "scion_a_total", &[]), Some(7.0));
        assert_eq!(
            TestCluster::metric(text, "scion_a_total", &[("tier", "daemon")]),
            Some(4.0)
        );
        assert_eq!(TestCluster::metric(text, "scion_b", &[]), Some(2.5));
        assert_eq!(TestCluster::metric(text, "scion_c", &[]), None);
        assert_eq!(
            TestCluster::metric(text, "scion_a_total", &[("tier", "x")]),
            None
        );
    }

    #[test]
    fn descriptor_lists_thirty_rpcs() {
        let rpcs = TestCluster::rpcs();
        assert_eq!(rpcs.len(), 30);
        assert!(
            rpcs.iter()
                .any(|r| r.path == "/scion.v1.WriteService/UploadObjects"
                    && r.client_streaming
                    && !r.server_streaming)
        );
    }

    #[test]
    fn default_acl_parses() {
        scion_policy::Policy::parse(TestCluster::DEFAULT_ACL).expect("valid default ACL");
    }
}
