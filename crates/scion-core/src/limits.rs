//! Repository limits (SPEC 6.1 `[limits]`, 6.5 defaults).

use serde::{Deserialize, Serialize};

/// SPEC 6.5 limits. Deserialized from `[limits]` with defaults and unknown-key rejection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Maximum blob size in bytes (default 104857600 = 100 MiB).
    pub max_blob_bytes: u64,
    /// Maximum delta entries per changeset (default 100000).
    pub max_delta_entries: u64,
    /// Maximum parents per changeset (default 16).
    pub max_parents: u64,
    /// Maximum Meta message size in bytes (default 1048576).
    pub max_message_bytes: u64,
    /// Maximum text size for content merges (default 10485760 = 10 MiB).
    pub merge_max_text_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_blob_bytes: 104_857_600,
            max_delta_entries: 100_000,
            max_parents: 16,
            max_message_bytes: 1_048_576,
            merge_max_text_bytes: 10_485_760,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_6_5_defaults() {
        let l = Limits::default();
        assert_eq!(l.max_blob_bytes, 100 * 1024 * 1024);
        assert_eq!(l.max_delta_entries, 100_000);
        assert_eq!(l.max_parents, 16);
        assert_eq!(l.max_message_bytes, 1_048_576);
        assert_eq!(l.merge_max_text_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn serde_defaults_and_unknown_keys() {
        let l: Limits = serde_json::from_str(r#"{"max_parents": 4}"#).unwrap();
        assert_eq!(l.max_parents, 4);
        assert_eq!(l.max_blob_bytes, Limits::default().max_blob_bytes);
        assert!(serde_json::from_str::<Limits>(r#"{"max_blobs": 1}"#).is_err());
    }
}
