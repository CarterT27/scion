//! SPEC 7.2: error details travel in `grpc-status-details-bin`; a large MERGE_CONFLICT report
//! (thousands of conflicts, hundreds of KiB) must survive a trailers-only response, which needs
//! the client to advertise a header list size above hyper's 16 KiB default.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::{Conflict, ConflictKind, ConflictReport, ErrorCode, ErrorPayload, ScionError};
use scion_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
use scion_proto::{error_from_status, status_from_error, v1};

#[derive(Clone)]
struct Big(usize);
#[tonic::async_trait]
impl ObjectService for Big {
    async fn get_server_info(
        &self,
        _r: tonic::Request<v1::GetServerInfoRequest>,
    ) -> Result<tonic::Response<v1::ServerInfo>, tonic::Status> {
        let rep = ConflictReport {
            clean_entries: vec![],
            conflicts: (0..self.0)
                .map(|i| Conflict {
                    path: format!("dir/file-{i:06}.txt").into_bytes(),
                    kind: ConflictKind::Content,
                    base: None,
                    ours: None,
                    theirs: None,
                    hunks: vec![],
                })
                .collect(),
        };
        Err(status_from_error(
            &ScionError::new(ErrorCode::MergeConflict, "c")
                .with_payload(ErrorPayload::MergeConflict(Box::new(rep))),
        ))
    }
    type GetObjectsStream = tokio_stream::Empty<Result<v1::ObjectChunk, tonic::Status>>;
    async fn get_objects(
        &self,
        _r: tonic::Request<v1::GetObjectsRequest>,
    ) -> Result<tonic::Response<Self::GetObjectsStream>, tonic::Status> {
        Ok(tonic::Response::new(tokio_stream::empty()))
    }
    async fn has_objects(
        &self,
        _r: tonic::Request<v1::HasObjectsRequest>,
    ) -> Result<tonic::Response<v1::HasObjectsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }
    async fn resolve_ref(
        &self,
        _r: tonic::Request<v1::ResolveRefRequest>,
    ) -> Result<tonic::Response<v1::ResolveRefResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn large_merge_conflict_survives_transport() {
    for n in [10usize, 1000, 10_000] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(ObjectServiceServer::new(Big(n)))
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
        );
        let c = scion_proto::ScionClient::connect(&scion_proto::Endpoint::Unix(path), None, None)
            .await
            .unwrap();
        let st = c
            .objects()
            .get_server_info(v1::GetServerInfoRequest {})
            .await
            .unwrap_err();
        assert_eq!(st.code(), tonic::Code::Aborted, "n={n}: {}", st.message());
        let e = error_from_status(&st);
        assert_eq!(e.code, ErrorCode::MergeConflict);
        match e.payload {
            ErrorPayload::MergeConflict(r) => assert_eq!(r.conflicts.len(), n),
            p => panic!("unexpected payload {p:?}"),
        }
        task.abort();
    }
}
