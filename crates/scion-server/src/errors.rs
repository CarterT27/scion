//! ScionError to tonic::Status mapping via scion-proto (SPEC 7.2; P23).
//!
//! Every handler error goes through [`to_status`], so the gRPC code, the `ErrorDetail`
//! (`code`, `retryable`, `message`, `payload`) and the retryable flag always follow the SPEC 7.2
//! table implemented by [`scion_proto::status_from_error`].
#![allow(dead_code)] // Helpers shared with handlers owned by other packages.

use scion_core::{ErrorCode, ScionError};

/// SPEC 7.2: the gRPC status for `e` (code per the table plus the `ErrorDetail` details).
pub fn to_status(e: &ScionError) -> tonic::Status {
    scion_proto::status_from_error(e)
}

/// SPEC 7.2: an `http::Response` carrying `e` as a trailers-only gRPC error, for tower layers
/// that reject a call before it reaches a handler (authentication, rate limits, shedding).
pub fn error_response<B: Default>(e: &ScionError) -> http::Response<B> {
    to_status(e).into_http()
}

/// Convenience conversion of `Result<T, ScionError>` into `Result<T, Status>` (SPEC 7.2).
pub trait StatusExt<T> {
    /// Map the error through [`to_status`].
    fn map_status(self) -> Result<T, tonic::Status>;
}

impl<T> StatusExt<T> for Result<T, ScionError> {
    fn map_status(self) -> Result<T, tonic::Status> {
        self.map_err(|e| to_status(&e))
    }
}

/// The SPEC 7.2 code name recorded in logs and in the `code` metric label of a response: `OK`
/// on success, the `ErrorDetail.code` when the status carries one, else the upper-case gRPC
/// code name (for example `UNIMPLEMENTED` from a stub handler).
pub fn status_label(s: Option<&tonic::Status>) -> String {
    let Some(s) = s else {
        return "OK".to_owned();
    };
    if s.code() == tonic::Code::Ok {
        return "OK".to_owned();
    }
    if !s.details().is_empty()
        && let Ok(d) = <scion_proto::v1::ErrorDetail as prost::Message>::decode(s.details())
        && let Some(code) = ErrorCode::parse(&d.code)
    {
        return code.as_str().to_owned();
    }
    grpc_code_name(s.code()).to_owned()
}

/// Upper-case snake name of a gRPC code (as in the gRPC specification).
pub fn grpc_code_name(c: tonic::Code) -> &'static str {
    match c {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "CANCELLED",
        tonic::Code::Unknown => "UNKNOWN",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        tonic::Code::NotFound => "NOT_FOUND",
        tonic::Code::AlreadyExists => "ALREADY_EXISTS",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
        tonic::Code::Aborted => "ABORTED",
        tonic::Code::OutOfRange => "OUT_OF_RANGE",
        tonic::Code::Unimplemented => "UNIMPLEMENTED",
        tonic::Code::Internal => "INTERNAL",
        tonic::Code::Unavailable => "UNAVAILABLE",
        tonic::Code::DataLoss => "DATA_LOSS",
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_maps_per_spec_table() {
        for code in ErrorCode::ALL {
            let e = ScionError::new(code, "m");
            let s = to_status(&e);
            assert_eq!(s.code(), scion_proto::tonic_code(code.grpc_code()));
            assert_eq!(status_label(Some(&s)), code.as_str());
            assert_eq!(
                scion_proto::error_from_status(&s).retryable(),
                code.retryable()
            );
        }
        let r: Result<(), ScionError> = Err(ScionError::not_found("x"));
        assert_eq!(r.map_status().unwrap_err().code(), tonic::Code::NotFound);
    }

    #[test]
    fn labels() {
        assert_eq!(status_label(None), "OK");
        assert_eq!(
            status_label(Some(&tonic::Status::unimplemented("P99"))),
            "UNIMPLEMENTED"
        );
        assert_eq!(
            status_label(Some(&tonic::Status::with_details(
                tonic::Code::NotFound,
                "x",
                bytes::Bytes::from_static(&[0xff, 0xff])
            ))),
            "NOT_FOUND"
        );
        let r: http::Response<tonic::body::Body> =
            error_response(&ScionError::resource_exhausted("slow down"));
        let s = tonic::Status::from_header_map(r.headers()).unwrap();
        assert_eq!(s.code(), tonic::Code::ResourceExhausted);
        assert_eq!(status_label(Some(&s)), "RESOURCE_EXHAUSTED");
    }
}
