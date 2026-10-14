//! SPEC 7.1 / 19.7: every response carries the three epoch trailers, over TLS and over a Unix
//! socket: unary success, unary error (trailers-only), server streaming, server streaming that
//! fails mid-stream, a trailers-only streaming error, client streaming (success and error).
//! `ScionClient` captures them into `last_epochs` and counts calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod proto_support;

use proto_support::{
    FAIL_MID_STREAM, TestServer, UPLOAD_FAIL_KIND, blob_id, start_tls, start_unix,
};
use scion_core::{ErrorCode, Tier};
use scion_proto::{
    Epochs, ScionClient, TRAILER_POLICY_EPOCH, TRAILER_REDACTION_EPOCH, TRAILER_TIER,
    error_from_status, read_epochs, v1,
};

fn ep(p: u64, r: u64) -> Epochs {
    Epochs {
        policy: p,
        redaction: r,
        tier: Tier::Server,
    }
}

fn assert_three(m: &tonic::metadata::MetadataMap, want: Epochs) {
    let h = m.clone().into_headers();
    for k in [TRAILER_POLICY_EPOCH, TRAILER_REDACTION_EPOCH, TRAILER_TIER] {
        assert!(h.contains_key(k), "missing {k} in {h:?}");
    }
    assert_eq!(read_epochs(&h), Some(want));
}

async fn suite(srv: &TestServer) {
    let client = ScionClient::connect(&srv.endpoint, srv.tls.clone(), Some("tok".into()))
        .await
        .unwrap();
    assert_eq!(client.calls(), 0);
    assert_eq!(client.last_epochs(), None);

    // Unary success: trailers merged into the response metadata.
    srv.epochs.set(10, 20);
    let resp = client
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    assert_three(resp.metadata(), ep(10, 20));
    assert_eq!(resp.get_ref().repo, "mini");
    assert_eq!(client.last_epochs(), Some(ep(10, 20)));

    // Unary error: trailers-only response; the keys are in the status metadata (headers).
    srv.epochs.set(11, 21);
    let st = client
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![blob_id(1)],
        })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::NotFound);
    assert_three(st.metadata(), ep(11, 21));
    assert_eq!(client.last_epochs(), Some(ep(11, 21)));

    // Server streaming success: trailers after the last message.
    srv.epochs.set(12, 22);
    let mut stream = client
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![blob_id(1), blob_id(2)],
            offset: None,
            length: None,
        })
        .await
        .unwrap()
        .into_inner();
    let mut n = 0;
    while let Some(c) = stream.message().await.unwrap() {
        assert!(c.r#final);
        n += 1;
    }
    assert_eq!(n, 2);
    let t = stream.trailers().await.unwrap().unwrap();
    assert_three(&t, ep(12, 22));
    assert_eq!(client.last_epochs(), Some(ep(12, 22)));

    // Server streaming that fails mid-stream: the error and the epochs share the trailers frame.
    srv.epochs.set(13, 23);
    let mut stream = client
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![blob_id(3)],
            offset: Some(FAIL_MID_STREAM),
            length: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(stream.message().await.unwrap().is_some());
    let st = stream.message().await.unwrap_err();
    let e = error_from_status(&st);
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert_eq!(e.subcode(), Some("injected"));
    assert_three(st.metadata(), ep(13, 23));
    assert_eq!(client.last_epochs(), Some(ep(13, 23)));

    // Trailers-only streaming error (handler fails before the stream starts).
    srv.epochs.set(14, 24);
    let st = client
        .objects()
        .get_objects(v1::GetObjectsRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&st).code, ErrorCode::NotFound);
    assert_three(st.metadata(), ep(14, 24));
    assert_eq!(client.last_epochs(), Some(ep(14, 24)));

    // Client streaming.
    srv.epochs.set(15, 25);
    let items: Vec<v1::UploadItem> = (0..5)
        .map(|i| v1::UploadItem {
            id: blob_id(i),
            kind: 0,
            data: vec![i; 10],
        })
        .collect();
    let resp = client
        .write()
        .upload_objects(tokio_stream::iter(items))
        .await
        .unwrap();
    assert_eq!(resp.get_ref().results.len(), 5);
    assert_three(resp.metadata(), ep(15, 25));
    assert_eq!(client.last_epochs(), Some(ep(15, 25)));

    // Client streaming that fails after reading part of the request stream.
    srv.epochs.set(17, 27);
    let items: Vec<v1::UploadItem> = (0..3)
        .map(|i| v1::UploadItem {
            id: blob_id(i),
            kind: if i == 1 { UPLOAD_FAIL_KIND } else { 0 },
            data: vec![i; 10],
        })
        .collect();
    let st = client
        .write()
        .upload_objects(tokio_stream::iter(items))
        .await
        .unwrap_err();
    let e = error_from_status(&st);
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some("missing_field"));
    assert_three(st.metadata(), ep(17, 27));
    assert_eq!(client.last_epochs(), Some(ep(17, 27)));

    // An unknown method on a served-by-nobody service still carries the trailers (UNIMPLEMENTED
    // is produced by the router inside the layer).
    srv.epochs.set(16, 26);
    let st = client
        .refs()
        .get_ref_log(v1::GetRefLogRequest::default())
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unimplemented);
    assert_three(st.metadata(), ep(16, 26));
    let st = client
        .write()
        .land(v1::LandRequest::default())
        .await
        .unwrap_err();
    assert_eq!(st.code(), tonic::Code::Unimplemented);
    assert_three(st.metadata(), ep(16, 26));

    // Call counter: 9 calls so far; shared by clones and with_token; reset works.
    assert_eq!(client.calls(), 9);
    let other = client.with_token(None);
    other
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap();
    assert_eq!(client.calls(), 10);
    assert_eq!(other.calls(), 10);
    client.reset_calls();
    assert_eq!(other.calls(), 0);
    let cloned = client.clone();
    cloned
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    assert_eq!(client.calls(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn trailers_over_tls() {
    let srv = start_tls().await;
    suite(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn trailers_over_unix_socket() {
    let srv = start_unix().await;
    suite(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_rejects_untrusted_ca() {
    let srv = start_tls().await;
    // A CA that did not sign the server certificate.
    let other = proto_support::certs();
    let tls = scion_proto::TlsOptions {
        ca_pem: Some(other.ca_pem.into_bytes()),
        domain: None,
    };
    match ScionClient::connect(&srv.endpoint, Some(tls), None).await {
        Err(e) => assert_eq!(e.code, ErrorCode::Unavailable),
        Ok(c) => {
            // Connection establishment may be lazy about the handshake: the call must fail.
            let st = c
                .objects()
                .get_server_info(v1::GetServerInfoRequest {})
                .await
                .unwrap_err();
            assert_eq!(error_from_status(&st).code, ErrorCode::Unavailable);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_domain_override() {
    let srv = start_tls().await;
    let scion_proto::Endpoint::Https(url) = &srv.endpoint else {
        panic!("https endpoint expected")
    };
    // Connect by IP; the certificate also lists 127.0.0.1, and a domain override of
    // "localhost" must verify as well.
    let by_ip = scion_proto::Endpoint::parse(&url.replace("localhost", "127.0.0.1")).unwrap();
    let mut tls = srv.tls.clone().unwrap();
    let c = ScionClient::connect(&by_ip, Some(tls.clone()), None)
        .await
        .unwrap();
    c.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    tls.domain = Some("localhost".into());
    let c = ScionClient::connect(&by_ip, Some(tls), None).await.unwrap();
    c.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_token_is_sent() {
    use scion_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Echo(Arc<Mutex<Vec<Option<String>>>>);
    #[tonic::async_trait]
    impl ObjectService for Echo {
        async fn get_server_info(
            &self,
            r: tonic::Request<v1::GetServerInfoRequest>,
        ) -> Result<tonic::Response<v1::ServerInfo>, tonic::Status> {
            let auth = r
                .metadata()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned());
            self.0.lock().unwrap().push(auth);
            Ok(tonic::Response::new(v1::ServerInfo::default()))
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

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let echo = Echo::default();
    let seen = echo.0.clone();
    let task = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ObjectServiceServer::new(echo))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    let c = ScionClient::connect(&scion_proto::Endpoint::Unix(path), None, Some("abc".into()))
        .await
        .unwrap();
    c.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    c.with_token(None)
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    c.with_token(Some("xyz".into()))
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    // No epoch layer on this server: last_epochs stays unset.
    assert_eq!(c.last_epochs(), None);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Some("Bearer abc".to_owned()),
            None,
            Some("Bearer xyz".to_owned())
        ]
    );
    task.abort();
}
