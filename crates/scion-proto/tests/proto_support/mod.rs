//! Shared helpers for scion-proto integration tests: rcgen certificates, a minimal
//! ObjectService + WriteService, and servers over TLS or a Unix socket wrapped in
//! `EpochTrailerLayer`. (scion-proto may not dev-depend on scion-testkit, PLAN 1.2.)
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod generators;

use bytes::Bytes;
use scion_core::{ScionError, Tier};
use scion_proto::v1;
use scion_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
use scion_proto::v1::write_service_server::{WriteService, WriteServiceServer};
use scion_proto::{EpochSource, EpochTrailerLayer, Epochs, TlsOptions, status_from_error};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

/// CA + server certificate for `localhost` / `127.0.0.1`.
pub struct Certs {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn certs() -> Certs {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "scion-proto test CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let mut params =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let key = KeyPair::generate().unwrap();
    let cert = params.signed_by(&key, &issuer).unwrap();
    Certs {
        ca_pem: ca_cert.pem(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    }
}

/// A mutable epoch source (tier server).
#[derive(Default)]
pub struct TestEpochs {
    pub policy: AtomicU64,
    pub redaction: AtomicU64,
}

impl TestEpochs {
    pub fn set(&self, p: u64, r: u64) {
        self.policy.store(p, Ordering::SeqCst);
        self.redaction.store(r, Ordering::SeqCst);
    }
}

impl EpochSource for TestEpochs {
    fn epochs(&self) -> Epochs {
        Epochs {
            policy: self.policy.load(Ordering::SeqCst),
            redaction: self.redaction.load(Ordering::SeqCst),
            tier: Tier::Server,
        }
    }
}

/// Marker offset: GetObjects streams one chunk per id, then fails mid-stream.
pub const FAIL_MID_STREAM: u64 = 1_000_001;

/// Minimal ObjectService:
/// - GetServerInfo: success.
/// - GetObjects: no ids => immediate NOT_FOUND (trailers-only); otherwise one chunk per id whose
///   data is the id bytes repeated `offset` times (default 1); offset == FAIL_MID_STREAM fails
///   after the chunks.
/// - HasObjects: always NOT_FOUND (unary error).
/// - ResolveRef: echoes the name length as the version.
#[derive(Clone, Default)]
pub struct MiniObjects;

type ChunkStream = Pin<Box<dyn Stream<Item = Result<v1::ObjectChunk, Status>> + Send>>;

#[tonic::async_trait]
impl ObjectService for MiniObjects {
    async fn get_server_info(
        &self,
        _r: Request<v1::GetServerInfoRequest>,
    ) -> Result<Response<v1::ServerInfo>, Status> {
        Ok(Response::new(v1::ServerInfo {
            protocol_versions: vec!["scion.v1".into()],
            repo: "mini".into(),
            policy_epoch: 1,
            redaction_epoch: 2,
            ..Default::default()
        }))
    }

    type GetObjectsStream = ChunkStream;

    async fn get_objects(
        &self,
        r: Request<v1::GetObjectsRequest>,
    ) -> Result<Response<ChunkStream>, Status> {
        let r = r.into_inner();
        if r.ids.is_empty() {
            return Err(status_from_error(&ScionError::not_found("no ids")));
        }
        let fail = r.offset == Some(FAIL_MID_STREAM);
        let reps = match r.offset {
            Some(n) if !fail && n > 0 => n as usize,
            _ => 1,
        };
        let mut items: Vec<Result<v1::ObjectChunk, Status>> = r
            .ids
            .iter()
            .map(|id| {
                Ok(v1::ObjectChunk {
                    id: id.clone(),
                    status: v1::ItemStatus::Ok as i32,
                    encoding: v1::Encoding::Raw as i32,
                    data: id.repeat(reps),
                    total_size: (id.len() * reps) as u64,
                    access_class: v1::AccessClass::Root as i32,
                    r#final: true,
                    offset: 0,
                })
            })
            .collect();
        if fail {
            items.push(Err(status_from_error(&ScionError::unavailable(
                "injected",
                "mid-stream failure",
            ))));
        }
        Ok(Response::new(Box::pin(tokio_stream::iter(items))))
    }

    async fn has_objects(
        &self,
        _r: Request<v1::HasObjectsRequest>,
    ) -> Result<Response<v1::HasObjectsResponse>, Status> {
        Err(status_from_error(&ScionError::not_found(
            "has_objects fails",
        )))
    }

    async fn resolve_ref(
        &self,
        r: Request<v1::ResolveRefRequest>,
    ) -> Result<Response<v1::ResolveRefResponse>, Status> {
        let r = r.into_inner();
        Ok(Response::new(v1::ResolveRefResponse {
            target: vec![7; 34],
            version: r.name.len() as u64,
        }))
    }
}

/// Minimal WriteService: UploadObjects (client streaming) acknowledges each item; everything else
/// is UNIMPLEMENTED.
#[derive(Clone, Default)]
pub struct MiniWrite;

#[tonic::async_trait]
impl WriteService for MiniWrite {
    async fn upload_objects(
        &self,
        r: Request<Streaming<v1::UploadItem>>,
    ) -> Result<Response<v1::UploadObjectsResponse>, Status> {
        let mut s = r.into_inner();
        let mut results = Vec::new();
        while let Some(item) = s.message().await? {
            results.push(v1::UploadResult {
                id: item.id,
                status: v1::ItemStatus::Ok as i32,
                ..Default::default()
            });
        }
        Ok(Response::new(v1::UploadObjectsResponse { results }))
    }
    async fn create_changeset(
        &self,
        _r: Request<v1::CreateChangesetRequest>,
    ) -> Result<Response<v1::CreateChangesetResponse>, Status> {
        Err(Status::unimplemented("test"))
    }
    async fn rebase(
        &self,
        _r: Request<v1::RebaseRequest>,
    ) -> Result<Response<v1::RebaseResponse>, Status> {
        Err(Status::unimplemented("test"))
    }
    async fn merge(
        &self,
        _r: Request<v1::MergeRequest>,
    ) -> Result<Response<v1::MergeResponse>, Status> {
        Err(Status::unimplemented("test"))
    }
    async fn land(
        &self,
        _r: Request<v1::LandRequest>,
    ) -> Result<Response<v1::LandResponse>, Status> {
        Err(Status::unimplemented("test"))
    }
}

/// A running test server.
pub struct TestServer {
    pub endpoint: scion_proto::Endpoint,
    pub tls: Option<TlsOptions>,
    pub epochs: Arc<TestEpochs>,
    _dir: Option<tempfile::TempDir>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn router(
    epochs: Arc<TestEpochs>,
    tls: Option<&Certs>,
) -> tonic::transport::server::Router<
    tower::layer::util::Stack<EpochTrailerLayer, tower::layer::util::Identity>,
> {
    let mut b = tonic::transport::Server::builder();
    if let Some(c) = tls {
        b = b
            .tls_config(tonic::transport::ServerTlsConfig::new().identity(
                tonic::transport::Identity::from_pem(&c.cert_pem, &c.key_pem),
            ))
            .unwrap();
    }
    b.layer(EpochTrailerLayer::new(epochs))
        .add_service(ObjectServiceServer::new(MiniObjects))
        .add_service(WriteServiceServer::new(MiniWrite))
}

/// Serve the mini services over TLS on 127.0.0.1:0.
pub async fn start_tls() -> TestServer {
    let c = certs();
    let epochs = Arc::new(TestEpochs::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = router(epochs.clone(), Some(&c));
    let task = tokio::spawn(async move {
        r.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    TestServer {
        endpoint: scion_proto::Endpoint::parse(&format!("https://localhost:{port}")).unwrap(),
        tls: Some(TlsOptions {
            ca_pem: Some(c.ca_pem.into_bytes()),
            domain: None,
        }),
        epochs,
        _dir: None,
        task,
    }
}

/// Serve the mini services over a Unix socket in a temp dir.
pub async fn start_unix() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let epochs = Arc::new(TestEpochs::default());
    let r = router(epochs.clone(), None);
    let task = tokio::spawn(async move {
        r.serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener))
            .await
            .unwrap();
    });
    TestServer {
        endpoint: scion_proto::Endpoint::Unix(path),
        tls: None,
        epochs,
        _dir: Some(dir),
        task,
    }
}

/// A 34-byte blob id with the given filler.
pub fn blob_id(n: u8) -> Vec<u8> {
    scion_core::ObjectId::new(scion_core::Kind::Blob, [n; 32])
        .as_bytes()
        .to_vec()
}

pub fn bytes_of(v: &[u8]) -> Bytes {
    Bytes::copy_from_slice(v)
}
