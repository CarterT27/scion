//! Configuration errors (SPEC 6: typed errors naming the key).

use scion_core::{ErrorCode, ScionError};

/// A configuration error (SPEC 6, 19.15).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ConfigError {
    /// An unknown key, as a full dotted path (e.g. `limits.max_blobs`).
    #[error("unknown configuration key {key}")]
    UnknownKey {
        /// Full dotted key path.
        key: String,
    },
    /// A `$NAME` value with no such environment variable.
    #[error("unresolved variable ${name} in {key}")]
    UnresolvedVar {
        /// Variable name.
        name: String,
        /// Key holding the reference.
        key: String,
    },
    /// An invalid value.
    #[error("invalid value for {key}: {msg}")]
    Invalid {
        /// Key.
        key: String,
        /// Explanation.
        msg: String,
    },
    /// TOML syntax error.
    #[error("parse error: {0}")]
    Parse(String),
    /// The file could not be read.
    #[error("io error: {0}")]
    Io(String),
}

impl ConfigError {
    /// Shorthand for [`ConfigError::Invalid`] (SPEC 6).
    pub fn invalid(key: impl Into<String>, msg: impl Into<String>) -> Self {
        ConfigError::Invalid {
            key: key.into(),
            msg: msg.into(),
        }
    }
}

/// Configuration errors surface as `INVALID_ARGUMENT` (SPEC 7.2; PLAN 5.1: local error enums
/// convert into `ScionError`).
impl From<ConfigError> for ScionError {
    fn from(e: ConfigError) -> Self {
        ScionError::new(ErrorCode::InvalidArgument, e.to_string())
    }
}

/// The name between the first pair of backticks after `prefix`, if `msg` starts with it.
fn backticked<'a>(msg: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = msg.strip_prefix(prefix)?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// Join a `serde_path_to_error` path (`.` at the root) with `field`, unless the path already
/// ends in that field.
fn full_key(path: &str, field: &str) -> String {
    if path.is_empty() || path == "." {
        field.to_string()
    } else if path == field || path.ends_with(&format!(".{field}")) {
        path.to_string()
    } else {
        format!("{path}.{field}")
    }
}

/// Map a deserialization error with its key path to a typed [`ConfigError`].
pub(crate) fn from_path_error(err: serde_path_to_error::Error<toml::de::Error>) -> ConfigError {
    let path = err.path().to_string();
    let msg = err.inner().message().trim().to_string();
    if let Some(field) = backticked(&msg, "unknown field `") {
        return ConfigError::UnknownKey {
            key: full_key(&path, field),
        };
    }
    if let Some(field) = backticked(&msg, "missing field `") {
        return ConfigError::Invalid {
            key: full_key(&path, field),
            msg: "required key is missing".to_string(),
        };
    }
    let key = if path == "." { String::new() } else { path };
    ConfigError::Invalid { key, msg }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_key_joins() {
        assert_eq!(full_key(".", "x"), "x");
        assert_eq!(full_key("limits", "x"), "limits.x");
        assert_eq!(full_key("limits.x", "x"), "limits.x");
        assert_eq!(full_key("x", "x"), "x");
        assert_eq!(full_key("a.bx", "x"), "a.bx.x");
    }

    #[test]
    fn backticked_extracts() {
        assert_eq!(
            backticked("unknown field `ab`, expected `c`", "unknown field `"),
            Some("ab")
        );
        assert_eq!(backticked("missing field `x`", "unknown field `"), None);
    }

    #[test]
    fn converts_to_scion_error() {
        let e: ScionError = ConfigError::UnknownKey { key: "a.b".into() }.into();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.message.contains("a.b"));
    }
}
