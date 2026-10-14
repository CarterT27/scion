//! SPEC 7.2 error mapping between [`ScionError`] and [`tonic::Status`] (P08).

use scion_core::{GrpcCode, ScionError};

/// Map a [`GrpcCode`] to the equivalent [`tonic::Code`].
pub fn tonic_code(c: GrpcCode) -> tonic::Code {
    let _ = c;
    todo!("P08")
}

/// SPEC 7.2: gRPC status from [`ScionError::code`] plus the `ErrorDetail` encoding in the status
/// details.
pub fn status_from_error(e: &ScionError) -> tonic::Status {
    let _ = e;
    todo!("P08")
}

/// Inverse of [`status_from_error`]. A status without details maps from its gRPC code; unknown
/// codes become UNAVAILABLE.
pub fn error_from_status(s: &tonic::Status) -> ScionError {
    let _ = s;
    todo!("P08")
}
