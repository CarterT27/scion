//! SPEC 7.2 / 19.7: every ErrorCode round-trips through `status_from_error` /
//! `error_from_status` with the correct tonic code and `retryable` flag, and every payload
//! variant round-trips, both in memory and over a real gRPC connection.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod proto_support;

use prost::Message;
use proto_support::generators::{self as g, PAYLOAD_VARIANTS, Rng};
use scion_core::test_support::proptest_cases;
use scion_core::{ErrorCode, ErrorPayload, GrpcCode, ScionError, subcode};
use scion_proto::{error_from_status, status_from_error, tonic_code, v1};

const SPEC_7_2: [(ErrorCode, tonic::Code, bool); 15] = [
    (ErrorCode::NotFound, tonic::Code::NotFound, false),
    (
        ErrorCode::PermissionDenied,
        tonic::Code::PermissionDenied,
        false,
    ),
    (
        ErrorCode::Unauthenticated,
        tonic::Code::Unauthenticated,
        false,
    ),
    (
        ErrorCode::InvalidArgument,
        tonic::Code::InvalidArgument,
        false,
    ),
    (ErrorCode::TooLarge, tonic::Code::InvalidArgument, false),
    (ErrorCode::Integrity, tonic::Code::DataLoss, true),
    (ErrorCode::Redacted, tonic::Code::FailedPrecondition, false),
    (ErrorCode::MergeConflict, tonic::Code::Aborted, false),
    (
        ErrorCode::AmbiguousMergeBase,
        tonic::Code::FailedPrecondition,
        false,
    ),
    (ErrorCode::RefVersionMismatch, tonic::Code::Aborted, true),
    (
        ErrorCode::NotFastForward,
        tonic::Code::FailedPrecondition,
        false,
    ),
    (ErrorCode::AlreadyLanded, tonic::Code::AlreadyExists, false),
    (
        ErrorCode::EmptyChange,
        tonic::Code::FailedPrecondition,
        false,
    ),
    (
        ErrorCode::ResourceExhausted,
        tonic::Code::ResourceExhausted,
        true,
    ),
    (ErrorCode::Unavailable, tonic::Code::Unavailable, true),
];

#[test]
fn tonic_code_matches_numeric_values() {
    for g in [
        GrpcCode::InvalidArgument,
        GrpcCode::NotFound,
        GrpcCode::AlreadyExists,
        GrpcCode::PermissionDenied,
        GrpcCode::ResourceExhausted,
        GrpcCode::FailedPrecondition,
        GrpcCode::Aborted,
        GrpcCode::Unimplemented,
        GrpcCode::Internal,
        GrpcCode::Unavailable,
        GrpcCode::DataLoss,
        GrpcCode::Unauthenticated,
    ] {
        assert_eq!(tonic_code(g) as i32, g.as_i32(), "{g:?}");
    }
}

#[test]
fn every_code_with_every_payload_round_trips() {
    let mut r = Rng::new(7);
    assert_eq!(ErrorCode::ALL.len(), SPEC_7_2.len());
    for (code, want_code, want_retry) in SPEC_7_2 {
        for which in 0..PAYLOAD_VARIANTS {
            let e = ScionError::new(code, format!("{code} with payload {which}"))
                .with_payload(g::payload_variant(&mut r, which));
            let s = status_from_error(&e);
            assert_eq!(s.code(), want_code, "{code}");
            assert_eq!(s.message(), e.message);
            let d = v1::ErrorDetail::decode(s.details()).unwrap();
            assert_eq!(d.code, code.as_str());
            assert_eq!(d.retryable, want_retry, "{code}");
            assert_eq!(d.message, e.message);
            assert_eq!(d.payload.is_some(), e.payload != ErrorPayload::None);
            let back = error_from_status(&s);
            assert_eq!(back, e);
            assert_eq!(back.retryable(), want_retry);
        }
    }
}

#[test]
fn payload_oneof_tags() {
    use v1::error_payload::Kind;
    let mut r = Rng::new(1);
    let tags: Vec<&str> = (1..PAYLOAD_VARIANTS)
        .map(|w| {
            match scion_proto::convert::payload_to_proto(&g::payload_variant(&mut r, w))
                .unwrap()
                .kind
                .unwrap()
            {
                Kind::Subcode(_) => "subcode",
                Kind::Limit(_) => "limit",
                Kind::MergeConflict(_) => "merge_conflict",
                Kind::RefMismatch(_) => "ref_mismatch",
                Kind::AlreadyLanded(_) => "already_landed",
                Kind::Candidates(_) => "candidates",
            }
        })
        .collect();
    assert_eq!(
        tags,
        [
            "subcode",
            "limit",
            "merge_conflict",
            "ref_mismatch",
            "already_landed",
            "candidates"
        ]
    );
    assert!(scion_proto::convert::payload_to_proto(&ErrorPayload::None).is_none());
}

#[test]
fn candidates_keep_ambiguous_prefix_subcode() {
    let mut r = Rng::new(3);
    let e = ScionError::new(ErrorCode::InvalidArgument, "ambiguous").with_payload(
        ErrorPayload::Candidates(vec![g::oid(&mut r), g::oid(&mut r)]),
    );
    let back = error_from_status(&status_from_error(&e));
    assert_eq!(back.subcode(), Some(subcode::AMBIGUOUS_PREFIX));
    assert_eq!(back, e);
}

#[test]
fn constructors_round_trip() {
    let id = scion_core::ObjectId::hash_blob(b"x");
    for e in [
        ScionError::invalid(subcode::MISSING_FIELD, "m"),
        ScionError::too_large("max_parents", 16, 17),
        ScionError::unavailable(subcode::UPSTREAM_UNAVAILABLE, "down"),
        ScionError::redacted(&id),
        ScionError::integrity(&id),
        ScionError::resource_exhausted("slow down"),
        ScionError::unauthenticated("no token"),
        ScionError::permission_denied("no"),
        ScionError::not_found("gone"),
    ] {
        assert_eq!(error_from_status(&status_from_error(&e)), e);
    }
}

#[test]
fn retryable_flag_is_derived_from_code() {
    // A peer claiming retryable=true for NOT_FOUND does not change the mapping.
    let d = v1::ErrorDetail {
        code: "NOT_FOUND".into(),
        retryable: true,
        message: "m".into(),
        payload: None,
    };
    let s = tonic::Status::with_details(
        tonic::Code::NotFound,
        "m",
        bytes::Bytes::from(d.encode_to_vec()),
    );
    let e = error_from_status(&s);
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(!e.retryable());
}

#[test]
fn malformed_detail_payload_is_dropped() {
    let d = v1::ErrorDetail {
        code: "ALREADY_LANDED".into(),
        retryable: false,
        message: "m".into(),
        payload: Some(v1::ErrorPayload {
            kind: Some(v1::error_payload::Kind::AlreadyLanded(
                v1::AlreadyLandedPayload {
                    source: vec![1, 2, 3],
                    result: vec![],
                    version: 1,
                },
            )),
        }),
    };
    let s = tonic::Status::with_details(
        tonic::Code::AlreadyExists,
        "m",
        bytes::Bytes::from(d.encode_to_vec()),
    );
    let e = error_from_status(&s);
    assert_eq!(e.code, ErrorCode::AlreadyLanded);
    assert_eq!(e.payload, ErrorPayload::None);
}

fn random_errors(cases: u32, seed: u64) {
    let mut r = Rng::new(seed);
    for _ in 0..cases {
        let e = g::error(&mut r);
        let s = status_from_error(&e);
        assert_eq!(s.code(), tonic_code(e.code.grpc_code()));
        assert_eq!(error_from_status(&s), e);
    }
}

#[test]
fn random_errors_round_trip() {
    random_errors(proptest_cases(2_000), 11);
}

#[test]
#[ignore = "spec-sized; run with scripts/check.sh --full"]
fn full_random_errors_round_trip() {
    let n = if scion_core::test_support::full_mode() {
        1_000_000
    } else {
        proptest_cases(10_000)
    };
    random_errors(n, 12);
}

/// Statuses survive the real transport (details travel in `grpc-status-details-bin`).
#[tokio::test(flavor = "multi_thread")]
async fn errors_round_trip_over_the_wire() {
    use scion_proto::v1::object_service_server::{ObjectService, ObjectServiceServer};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Failing(Arc<Mutex<Vec<ScionError>>>);
    #[tonic::async_trait]
    impl ObjectService for Failing {
        async fn get_server_info(
            &self,
            _r: tonic::Request<v1::GetServerInfoRequest>,
        ) -> Result<tonic::Response<v1::ServerInfo>, tonic::Status> {
            let e = self.0.lock().unwrap().remove(0);
            Err(status_from_error(&e))
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

    let mut r = Rng::new(99);
    let mut errors = Vec::new();
    for code in ErrorCode::ALL {
        for which in 0..PAYLOAD_VARIANTS {
            errors.push(
                ScionError::new(code, format!("over the wire: {code} é 日 %"))
                    .with_payload(g::payload_variant(&mut r, which)),
            );
        }
    }
    let queue = Arc::new(Mutex::new(errors.clone()));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let task = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ObjectServiceServer::new(Failing(queue)))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    let c = scion_proto::ScionClient::connect(&scion_proto::Endpoint::Unix(path), None, None)
        .await
        .unwrap();
    for want in errors {
        let st = c
            .objects()
            .get_server_info(v1::GetServerInfoRequest {})
            .await
            .unwrap_err();
        assert_eq!(st.code(), tonic_code(want.code.grpc_code()));
        assert_eq!(error_from_status(&st), want);
    }
    task.abort();
}
