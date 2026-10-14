//! Policy validation errors (SPEC 6.2).

use scion_core::{ErrorCode, ScionError};

/// Why an `acl.toml` is invalid.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PolicyError {
    /// TOML syntax or type error.
    #[error("policy toml: {0}")]
    Toml(String),
    /// An unknown key (full dotted path).
    #[error("unknown policy key {0}")]
    UnknownKey(String),
    /// `version` is not 1.
    #[error("unsupported policy version {0}")]
    UnsupportedVersion(i64),
    /// No `/` rule (REQUIRED).
    #[error("policy has no \"/\" rule")]
    MissingRootRule,
    /// A group membership cycle.
    #[error("group cycle: {0:?}")]
    GroupCycle(Vec<String>),
    /// A reference to an undefined group.
    #[error("unknown group {0}")]
    UnknownGroup(String),
    /// A malformed principal.
    #[error("bad principal {0}")]
    BadPrincipal(String),
    /// A malformed path prefix.
    #[error("bad prefix {0}")]
    BadPrefix(String),
    /// A malformed ref pattern.
    #[error("bad ref pattern {0}")]
    BadRefPattern(String),
    /// The file could not be read.
    #[error("policy io: {0}")]
    Io(String),
}

impl From<PolicyError> for ScionError {
    fn from(e: PolicyError) -> Self {
        ScionError::new(ErrorCode::InvalidArgument, e.to_string())
    }
}
