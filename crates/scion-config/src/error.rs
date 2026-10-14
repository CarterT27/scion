//! Configuration errors (SPEC 6: typed errors naming the key).

/// A configuration error.
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
