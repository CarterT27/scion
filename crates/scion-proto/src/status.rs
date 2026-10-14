//! SPEC 7.2 error mapping between [`ScionError`] and [`tonic::Status`] (P08).
//!
//! The gRPC status code comes from [`ErrorCode::grpc_code`]; the status message is the error
//! message; the status details carry the protobuf encoding of `scion.v1.ErrorDetail`
//! (`code`, `retryable`, `message`, `payload`).

use crate::v1;
use prost::Message;
use scion_core::{ErrorCode, ErrorPayload, GrpcCode, ScionError};

/// Map a [`GrpcCode`] to the equivalent [`tonic::Code`] (SPEC 7.2).
pub fn tonic_code(c: GrpcCode) -> tonic::Code {
    match c {
        GrpcCode::InvalidArgument => tonic::Code::InvalidArgument,
        GrpcCode::NotFound => tonic::Code::NotFound,
        GrpcCode::AlreadyExists => tonic::Code::AlreadyExists,
        GrpcCode::PermissionDenied => tonic::Code::PermissionDenied,
        GrpcCode::ResourceExhausted => tonic::Code::ResourceExhausted,
        GrpcCode::FailedPrecondition => tonic::Code::FailedPrecondition,
        GrpcCode::Aborted => tonic::Code::Aborted,
        GrpcCode::Unimplemented => tonic::Code::Unimplemented,
        GrpcCode::Internal => tonic::Code::Internal,
        GrpcCode::Unavailable => tonic::Code::Unavailable,
        GrpcCode::DataLoss => tonic::Code::DataLoss,
        GrpcCode::Unauthenticated => tonic::Code::Unauthenticated,
    }
}

/// SPEC 7.2: gRPC status from [`ScionError::code`] plus the `ErrorDetail` encoding in the status
/// details.
pub fn status_from_error(e: &ScionError) -> tonic::Status {
    let detail = v1::ErrorDetail::from(e);
    tonic::Status::with_details(
        tonic_code(e.code.grpc_code()),
        e.message.clone(),
        bytes::Bytes::from(detail.encode_to_vec()),
    )
}

/// The SPEC 7.2 code for a status that carries no (usable) `ErrorDetail`: only unambiguous
/// gRPC codes map back; everything else is UNAVAILABLE.
fn code_from_grpc(c: tonic::Code) -> ErrorCode {
    match c {
        tonic::Code::NotFound => ErrorCode::NotFound,
        tonic::Code::PermissionDenied => ErrorCode::PermissionDenied,
        tonic::Code::Unauthenticated => ErrorCode::Unauthenticated,
        tonic::Code::InvalidArgument => ErrorCode::InvalidArgument,
        tonic::Code::DataLoss => ErrorCode::Integrity,
        tonic::Code::AlreadyExists => ErrorCode::AlreadyLanded,
        tonic::Code::ResourceExhausted => ErrorCode::ResourceExhausted,
        _ => ErrorCode::Unavailable,
    }
}

/// Inverse of [`status_from_error`]. A status without details maps from its gRPC code; unknown
/// codes become UNAVAILABLE.
///
/// When the details decode as an `ErrorDetail` with a known code, the code, message and payload
/// come from it (a malformed payload is dropped rather than failing the mapping). Otherwise the
/// code is derived from the gRPC status code, with the status message.
pub fn error_from_status(s: &tonic::Status) -> ScionError {
    if !s.details().is_empty()
        && let Ok(detail) = v1::ErrorDetail::decode(s.details())
        && let Some(code) = ErrorCode::parse(&detail.code)
    {
        let payload = match crate::convert::payload_from_proto(detail.payload.as_ref()) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "dropping malformed ErrorDetail payload");
                ErrorPayload::None
            }
        };
        return ScionError {
            code,
            message: detail.message,
            payload,
        };
    }
    let code = code_from_grpc(s.code());
    let message = if s.message().is_empty() {
        format!("gRPC status {:?}", s.code())
    } else {
        s.message().to_owned()
    };
    ScionError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::subcode;

    #[test]
    fn every_code_round_trips() {
        for code in ErrorCode::ALL {
            let e = ScionError::new(code, format!("m-{code}"));
            let s = status_from_error(&e);
            assert_eq!(s.code(), tonic_code(code.grpc_code()));
            assert_eq!(s.code() as i32, code.grpc_code().as_i32());
            assert_eq!(s.message(), e.message);
            let d = v1::ErrorDetail::decode(s.details()).unwrap();
            assert_eq!(d.retryable, code.retryable());
            assert_eq!(d.code, code.as_str());
            assert_eq!(error_from_status(&s), e);
        }
    }

    #[test]
    fn statuses_without_details() {
        let cases = [
            (tonic::Code::NotFound, ErrorCode::NotFound),
            (tonic::Code::PermissionDenied, ErrorCode::PermissionDenied),
            (tonic::Code::Unauthenticated, ErrorCode::Unauthenticated),
            (tonic::Code::InvalidArgument, ErrorCode::InvalidArgument),
            (tonic::Code::DataLoss, ErrorCode::Integrity),
            (tonic::Code::AlreadyExists, ErrorCode::AlreadyLanded),
            (tonic::Code::ResourceExhausted, ErrorCode::ResourceExhausted),
            (tonic::Code::Unavailable, ErrorCode::Unavailable),
            (tonic::Code::Unimplemented, ErrorCode::Unavailable),
            (tonic::Code::Internal, ErrorCode::Unavailable),
            (tonic::Code::Unknown, ErrorCode::Unavailable),
            (tonic::Code::DeadlineExceeded, ErrorCode::Unavailable),
            (tonic::Code::Cancelled, ErrorCode::Unavailable),
        ];
        for (c, want) in cases {
            let e = error_from_status(&tonic::Status::new(c, "boom"));
            assert_eq!(e.code, want, "{c:?}");
            assert_eq!(e.message, "boom");
            assert_eq!(e.payload, ErrorPayload::None);
        }
        // Garbage details fall back to the gRPC code.
        let s = tonic::Status::with_details(
            tonic::Code::NotFound,
            "x",
            bytes::Bytes::from_static(&[0xff, 0xff, 0xff]),
        );
        assert_eq!(error_from_status(&s).code, ErrorCode::NotFound);
    }

    #[test]
    fn subcode_payload_round_trips() {
        let e = ScionError::invalid(subcode::MISSING_FIELD, "missing");
        let back = error_from_status(&status_from_error(&e));
        assert_eq!(back, e);
        assert_eq!(back.subcode(), Some(subcode::MISSING_FIELD));
    }
}
