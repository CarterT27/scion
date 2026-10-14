//! The cross-crate error model (SPEC 7.2, PLAN 2.1).

use crate::conflict::ConflictReport;
use crate::id::ObjectId;
use crate::subcode;

/// SPEC 7.2 error codes. Every Scion error carries exactly one of these.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Object, changeset, path, or ref absent.
    NotFound,
    /// Policy denies.
    PermissionDenied,
    /// Missing, invalid, or expired token.
    Unauthenticated,
    /// Validation failure; `payload.subcode` names the rule.
    InvalidArgument,
    /// A configured limit exceeded.
    TooLarge,
    /// Bytes do not match identifier.
    Integrity,
    /// Object is redacted.
    Redacted,
    /// Merge produced conflicts; payload lists them (SPEC 8.3).
    MergeConflict,
    /// Multiple best common ancestors.
    AmbiguousMergeBase,
    /// CAS lost; payload has current version and target.
    RefVersionMismatch,
    /// Update would drop history.
    NotFastForward,
    /// ChangeId landed from a different source changeset.
    AlreadyLanded,
    /// Rebase result has an empty delta.
    EmptyChange,
    /// Rate limited or shedding load.
    ResourceExhausted,
    /// Dependency down or timeout.
    Unavailable,
}

impl ErrorCode {
    /// All fifteen SPEC 7.2 codes, in table order.
    pub const ALL: [ErrorCode; 15] = [
        ErrorCode::NotFound,
        ErrorCode::PermissionDenied,
        ErrorCode::Unauthenticated,
        ErrorCode::InvalidArgument,
        ErrorCode::TooLarge,
        ErrorCode::Integrity,
        ErrorCode::Redacted,
        ErrorCode::MergeConflict,
        ErrorCode::AmbiguousMergeBase,
        ErrorCode::RefVersionMismatch,
        ErrorCode::NotFastForward,
        ErrorCode::AlreadyLanded,
        ErrorCode::EmptyChange,
        ErrorCode::ResourceExhausted,
        ErrorCode::Unavailable,
    ];

    /// The SPEC 7.2 `code` string, e.g. `"NOT_FOUND"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::PermissionDenied => "PERMISSION_DENIED",
            ErrorCode::Unauthenticated => "UNAUTHENTICATED",
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::TooLarge => "TOO_LARGE",
            ErrorCode::Integrity => "INTEGRITY",
            ErrorCode::Redacted => "REDACTED",
            ErrorCode::MergeConflict => "MERGE_CONFLICT",
            ErrorCode::AmbiguousMergeBase => "AMBIGUOUS_MERGE_BASE",
            ErrorCode::RefVersionMismatch => "REF_VERSION_MISMATCH",
            ErrorCode::NotFastForward => "NOT_FAST_FORWARD",
            ErrorCode::AlreadyLanded => "ALREADY_LANDED",
            ErrorCode::EmptyChange => "EMPTY_CHANGE",
            ErrorCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorCode::Unavailable => "UNAVAILABLE",
        }
    }

    /// Inverse of [`ErrorCode::as_str`] (SPEC 7.2).
    pub fn parse(s: &str) -> Option<Self> {
        ErrorCode::ALL.iter().copied().find(|c| c.as_str() == s)
    }

    /// The gRPC status for this code, exactly as the SPEC 7.2 table.
    pub fn grpc_code(self) -> GrpcCode {
        match self {
            ErrorCode::NotFound => GrpcCode::NotFound,
            ErrorCode::PermissionDenied => GrpcCode::PermissionDenied,
            ErrorCode::Unauthenticated => GrpcCode::Unauthenticated,
            ErrorCode::InvalidArgument | ErrorCode::TooLarge => GrpcCode::InvalidArgument,
            ErrorCode::Integrity => GrpcCode::DataLoss,
            ErrorCode::Redacted
            | ErrorCode::AmbiguousMergeBase
            | ErrorCode::NotFastForward
            | ErrorCode::EmptyChange => GrpcCode::FailedPrecondition,
            ErrorCode::MergeConflict | ErrorCode::RefVersionMismatch => GrpcCode::Aborted,
            ErrorCode::AlreadyLanded => GrpcCode::AlreadyExists,
            ErrorCode::ResourceExhausted => GrpcCode::ResourceExhausted,
            ErrorCode::Unavailable => GrpcCode::Unavailable,
        }
    }

    /// SPEC 7.2 `Retryable` column: true only for INTEGRITY, REF_VERSION_MISMATCH,
    /// RESOURCE_EXHAUSTED and UNAVAILABLE.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::Integrity
                | ErrorCode::RefVersionMismatch
                | ErrorCode::ResourceExhausted
                | ErrorCode::Unavailable
        )
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// gRPC status codes used by the SPEC 7.2 mapping (numeric values per the gRPC spec).
#[repr(i32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrpcCode {
    /// `INVALID_ARGUMENT` (3).
    InvalidArgument = 3,
    /// `NOT_FOUND` (5).
    NotFound = 5,
    /// `ALREADY_EXISTS` (6).
    AlreadyExists = 6,
    /// `PERMISSION_DENIED` (7).
    PermissionDenied = 7,
    /// `RESOURCE_EXHAUSTED` (8).
    ResourceExhausted = 8,
    /// `FAILED_PRECONDITION` (9).
    FailedPrecondition = 9,
    /// `ABORTED` (10).
    Aborted = 10,
    /// `UNIMPLEMENTED` (12).
    Unimplemented = 12,
    /// `INTERNAL` (13).
    Internal = 13,
    /// `UNAVAILABLE` (14).
    Unavailable = 14,
    /// `DATA_LOSS` (15).
    DataLoss = 15,
    /// `UNAUTHENTICATED` (16).
    Unauthenticated = 16,
}

impl GrpcCode {
    /// The numeric gRPC status code.
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// The single cross-crate error (SPEC 7.2): a code, a human message and a typed payload.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct ScionError {
    /// SPEC 7.2 code.
    pub code: ErrorCode,
    /// Human-readable message. Never contains secrets, tokens, content or raw paths.
    pub message: String,
    /// Typed payload (`ErrorDetail.payload`).
    pub payload: ErrorPayload,
}

/// `ErrorDetail.payload` (SPEC 7.2): at most one typed payload per error.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ErrorPayload {
    /// No payload.
    #[default]
    None,
    /// INVALID_ARGUMENT / UNAVAILABLE subcodes (see [`crate::subcode`]).
    Subcode(String),
    /// TOO_LARGE: the limit name (config key), its value and the actual value.
    Limit {
        /// Config key name, e.g. `"max_parents"`.
        name: String,
        /// Configured limit.
        limit: u64,
        /// Observed value.
        actual: u64,
    },
    /// MERGE_CONFLICT: the conflict report (SPEC 8.3).
    MergeConflict(Box<ConflictReport>),
    /// REF_VERSION_MISMATCH: the current ref state (SPEC 8.5).
    RefMismatch {
        /// Current version (0 when the ref is absent).
        current_version: u64,
        /// Current target (absent when the ref does not exist).
        current_target: Option<ObjectId>,
    },
    /// ALREADY_LANDED: the recorded landing of this change id (SPEC 8.4).
    AlreadyLanded {
        /// Source changeset recorded in `landed`.
        source: ObjectId,
        /// Result changeset recorded in `landed`.
        result: ObjectId,
        /// Ref version of that landing.
        version: u64,
    },
    /// Ambiguous changeset prefix candidates (PLAN 6.6).
    Candidates(Vec<ObjectId>),
}

impl ScionError {
    /// A new error with no payload.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ScionError {
            code,
            message: message.into(),
            payload: ErrorPayload::None,
        }
    }

    /// Replace the payload.
    pub fn with_payload(mut self, p: ErrorPayload) -> Self {
        self.payload = p;
        self
    }

    /// INVALID_ARGUMENT with a [`crate::subcode`] payload.
    pub fn invalid(subcode: &str, message: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::InvalidArgument, message)
            .with_payload(ErrorPayload::Subcode(subcode.to_owned()))
    }

    /// TOO_LARGE with a Limit payload naming the config key (PLAN 6.7).
    pub fn too_large(name: &str, limit: u64, actual: u64) -> Self {
        ScionError::new(
            ErrorCode::TooLarge,
            format!("limit {name} exceeded: {actual} > {limit}"),
        )
        .with_payload(ErrorPayload::Limit {
            name: name.to_owned(),
            limit,
            actual,
        })
    }

    /// NOT_FOUND.
    pub fn not_found(m: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::NotFound, m)
    }

    /// PERMISSION_DENIED.
    pub fn permission_denied(m: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::PermissionDenied, m)
    }

    /// UNAUTHENTICATED.
    pub fn unauthenticated(m: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::Unauthenticated, m)
    }

    /// UNAVAILABLE with a [`crate::subcode`] payload.
    pub fn unavailable(subcode: &str, m: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::Unavailable, m)
            .with_payload(ErrorPayload::Subcode(subcode.to_owned()))
    }

    /// RESOURCE_EXHAUSTED.
    pub fn resource_exhausted(m: impl Into<String>) -> Self {
        ScionError::new(ErrorCode::ResourceExhausted, m)
    }

    /// REDACTED for the given object (SPEC 13.4).
    pub fn redacted(id: &ObjectId) -> Self {
        ScionError::new(ErrorCode::Redacted, format!("object {id} is redacted"))
    }

    /// INTEGRITY for the given object (SPEC 7.2, 16.1 class 7).
    pub fn integrity(id: &ObjectId) -> Self {
        ScionError::new(
            ErrorCode::Integrity,
            format!("stored bytes of object {id} do not match its identifier"),
        )
    }

    /// The subcode, when the payload carries one. A `Candidates` payload reports
    /// [`subcode::AMBIGUOUS_PREFIX`].
    pub fn subcode(&self) -> Option<&str> {
        match &self.payload {
            ErrorPayload::Subcode(s) => Some(s.as_str()),
            ErrorPayload::Candidates(_) => Some(subcode::AMBIGUOUS_PREFIX),
            _ => None,
        }
    }

    /// `== self.code.retryable()` (SPEC 7.2).
    pub fn retryable(&self) -> bool {
        self.code.retryable()
    }
}

/// Result alias used by every public fallible API in the workspace (PLAN 5.1).
pub type Result<T, E = ScionError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_7_2_table() {
        use ErrorCode::*;
        use GrpcCode as G;
        let table: [(ErrorCode, &str, GrpcCode, bool); 15] = [
            (NotFound, "NOT_FOUND", G::NotFound, false),
            (
                PermissionDenied,
                "PERMISSION_DENIED",
                G::PermissionDenied,
                false,
            ),
            (
                Unauthenticated,
                "UNAUTHENTICATED",
                G::Unauthenticated,
                false,
            ),
            (
                InvalidArgument,
                "INVALID_ARGUMENT",
                G::InvalidArgument,
                false,
            ),
            (TooLarge, "TOO_LARGE", G::InvalidArgument, false),
            (Integrity, "INTEGRITY", G::DataLoss, true),
            (Redacted, "REDACTED", G::FailedPrecondition, false),
            (MergeConflict, "MERGE_CONFLICT", G::Aborted, false),
            (
                AmbiguousMergeBase,
                "AMBIGUOUS_MERGE_BASE",
                G::FailedPrecondition,
                false,
            ),
            (RefVersionMismatch, "REF_VERSION_MISMATCH", G::Aborted, true),
            (
                NotFastForward,
                "NOT_FAST_FORWARD",
                G::FailedPrecondition,
                false,
            ),
            (AlreadyLanded, "ALREADY_LANDED", G::AlreadyExists, false),
            (EmptyChange, "EMPTY_CHANGE", G::FailedPrecondition, false),
            (
                ResourceExhausted,
                "RESOURCE_EXHAUSTED",
                G::ResourceExhausted,
                true,
            ),
            (Unavailable, "UNAVAILABLE", G::Unavailable, true),
        ];
        assert_eq!(ErrorCode::ALL.len(), table.len());
        for (i, (code, s, g, r)) in table.iter().enumerate() {
            assert_eq!(ErrorCode::ALL[i], *code);
            assert_eq!(code.as_str(), *s);
            assert_eq!(ErrorCode::parse(s), Some(*code));
            assert_eq!(code.grpc_code(), *g, "{s}");
            assert_eq!(code.retryable(), *r, "{s}");
            assert_eq!(ScionError::new(*code, "x").retryable(), *r);
        }
        assert_eq!(ErrorCode::parse("BOGUS"), None);
    }

    #[test]
    fn grpc_numeric_values() {
        assert_eq!(GrpcCode::InvalidArgument.as_i32(), 3);
        assert_eq!(GrpcCode::NotFound as i32, 5);
        assert_eq!(GrpcCode::AlreadyExists as i32, 6);
        assert_eq!(GrpcCode::PermissionDenied as i32, 7);
        assert_eq!(GrpcCode::ResourceExhausted as i32, 8);
        assert_eq!(GrpcCode::FailedPrecondition as i32, 9);
        assert_eq!(GrpcCode::Aborted as i32, 10);
        assert_eq!(GrpcCode::Unimplemented as i32, 12);
        assert_eq!(GrpcCode::Internal as i32, 13);
        assert_eq!(GrpcCode::Unavailable as i32, 14);
        assert_eq!(GrpcCode::DataLoss as i32, 15);
        assert_eq!(GrpcCode::Unauthenticated as i32, 16);
    }

    #[test]
    fn constructors_and_display() {
        let e = ScionError::invalid(subcode::NOOP_PUT, "no-op put");
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some("noop_put"));
        assert_eq!(e.to_string(), "INVALID_ARGUMENT: no-op put");

        let e = ScionError::too_large("max_parents", 16, 17);
        assert_eq!(e.code, ErrorCode::TooLarge);
        assert_eq!(
            e.payload,
            ErrorPayload::Limit {
                name: "max_parents".into(),
                limit: 16,
                actual: 17
            }
        );
        assert_eq!(e.subcode(), None);

        let e = ScionError::unavailable(subcode::TXN_RETRY, "retry");
        assert!(e.retryable());
        assert_eq!(e.subcode(), Some(subcode::TXN_RETRY));

        let id = crate::id::empty_tree_id();
        let e = ScionError::redacted(&id);
        assert_eq!(e.code, ErrorCode::Redacted);
        assert!(e.message.contains(&id.to_text()));
        assert_eq!(ScionError::integrity(&id).code, ErrorCode::Integrity);
        assert_eq!(ScionError::not_found("x").code, ErrorCode::NotFound);
        assert_eq!(
            ScionError::permission_denied("x").code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            ScionError::unauthenticated("x").code,
            ErrorCode::Unauthenticated
        );
        assert_eq!(
            ScionError::resource_exhausted("x").code,
            ErrorCode::ResourceExhausted
        );

        let e = ScionError::invalid(subcode::AMBIGUOUS_PREFIX, "ambiguous")
            .with_payload(ErrorPayload::Candidates(vec![id]));
        assert_eq!(e.subcode(), Some(subcode::AMBIGUOUS_PREFIX));
    }
}
