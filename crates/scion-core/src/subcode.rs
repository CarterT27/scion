//! Frozen `payload.subcode` string constants (SPEC 7.2, 8.1, 4.1; PLAN 2.1).
//!
//! These are the only values ever placed in [`crate::ErrorPayload::Subcode`]. Never spell a
//! subcode as a string literal elsewhere; use these constants.

// ---- decoding (SPEC 4.1) ----
/// SPEC 4.1: a varint was not minimally encoded.
pub const NON_MINIMAL_VARINT: &str = "non_minimal_varint";
/// SPEC 4.1: a varint exceeded 10 bytes or overflowed 64 bits.
pub const VARINT_OVERFLOW: &str = "varint_overflow";
/// SPEC 4.1: a length or count exceeded the remaining input.
pub const LENGTH_OVERRUN: &str = "length_overrun";
/// SPEC 4.1: bytes remained after a complete value was decoded.
pub const TRAILING_BYTES: &str = "trailing_bytes";
/// SPEC 4.1: a list violated its ordering rule.
pub const UNSORTED: &str = "unsorted";
/// SPEC 4.1: a list violated its uniqueness rule.
pub const DUPLICATE: &str = "duplicate";
/// SPEC 4.1: an unknown `format_version`.
pub const UNKNOWN_FORMAT_VERSION: &str = "unknown_format_version";
/// SPEC 4.1: an unknown odd (critical) extension tag.
pub const UNKNOWN_CRITICAL_EXTENSION: &str = "unknown_critical_extension";
/// SPEC 4.3.2: a mode byte outside 1..=4 (or outside 1..=3 where only files are allowed).
pub const INVALID_MODE: &str = "invalid_mode";
/// SPEC 4.2: an unknown or mismatched object kind.
pub const INVALID_KIND: &str = "invalid_kind";
/// SPEC 4.2: an unknown hash algorithm byte.
pub const INVALID_ALGO: &str = "invalid_algo";
/// SPEC 4.3.2: a non-root tree without entries.
pub const EMPTY_TREE: &str = "empty_tree";
/// SPEC 4.2: malformed identifier text (or malformed raw identifier bytes).
pub const BAD_ID_TEXT: &str = "bad_id_text";

// ---- CreateChangeset (SPEC 8.1), in validation order ----
/// SPEC 8.1 step 1: a parent changeset does not exist.
pub const MISSING_PARENT: &str = "missing_parent";
/// SPEC 8.1 step 1: a parent is listed twice.
pub const DUPLICATE_PARENT: &str = "duplicate_parent";
/// SPEC 8.1 step 1: empty parents while a root changeset already exists.
pub const PARENTLESS_NOT_ALLOWED: &str = "parentless_not_allowed";
/// SPEC 8.1 step 2: delta entries not strictly ascending by path.
pub const DELTA_UNSORTED: &str = "delta_unsorted";
/// SPEC 8.1 step 2: a delta path appears twice.
pub const DELTA_DUPLICATE: &str = "delta_duplicate";
/// SPEC 4.4 / 8.1 step 2: a path violates the path rules.
pub const INVALID_PATH: &str = "invalid_path";
/// SPEC 8.1 step 3: a Put blob does not exist or is not live.
pub const MISSING_BLOB: &str = "missing_blob";
/// SPEC 8.1 step 3: a Put size differs from the stored size.
pub const SIZE_MISMATCH: &str = "size_mismatch";
/// SPEC 8.1 step 5: a Delete of a path absent in T1.
pub const DELETE_ABSENT: &str = "delete_absent";
/// SPEC 8.1 step 5: a Delete of a path that is not a file in T1.
pub const DELETE_NOT_FILE: &str = "delete_not_file";
/// SPEC 8.1 step 5: a Put identical to T1 at that path.
pub const NOOP_PUT: &str = "noop_put";
/// SPEC 8.1 step 6: file/directory inconsistency.
pub const FILE_DIRECTORY: &str = "file_directory";
/// SPEC 8.1: `expected_root_tree` differs from the built tree.
pub const ROOT_TREE_MISMATCH: &str = "root_tree_mismatch";
/// PLAN 6.7: a required protobuf field is missing.
pub const MISSING_FIELD: &str = "missing_field";

// ---- refs / land / rebase ----
/// SPEC 4.3.6: a ref name violates the grammar.
pub const INVALID_REF_NAME: &str = "invalid_ref_name";
/// SPEC 8.2 / PLAN 6.7: rebase of a changeset with more than one parent.
pub const MULTI_PARENT: &str = "multi_parent";
/// SPEC 8.4 / PLAN 6.7: a land stack contains a merge.
pub const MERGE_IN_STACK: &str = "merge_in_stack";
/// PLAN 6.7: `land.max_attempts` exhausted.
pub const LAND_CONTENTION: &str = "land_contention";
/// PLAN 6.7: creating a ref that already exists.
pub const REF_EXISTS: &str = "ref_exists";

// ---- revisions (SPEC 11.8) ----
/// SPEC 11.8: a revision string does not parse.
pub const INVALID_REVISION: &str = "invalid_revision";
/// SPEC 4.2 / PLAN 6.6: a changeset prefix matches more than one changeset.
pub const AMBIGUOUS_PREFIX: &str = "ambiguous_prefix";

// ---- redaction (SPEC 13) ----
/// PLAN 6.7: redacting an object that is not a Blob or Meta.
pub const NOT_REDACTABLE: &str = "not_redactable";
/// SPEC 7.7 / PLAN 6.7: unredact of an object that is not quarantined.
pub const NOT_QUARANTINED: &str = "not_quarantined";

// ---- infrastructure ----
/// SPEC 19.0: an injected fault.
pub const INJECTED: &str = "injected";
/// SPEC 19.0: an injected crash (the registry latches until reset).
pub const INJECTED_CRASH: &str = "injected_crash";
/// SPEC 5.2: serializable transaction retries exhausted.
pub const TXN_RETRY: &str = "txn_retry";
/// SPEC 16.1: the metadata or object store is unreachable.
pub const STORE_UNAVAILABLE: &str = "store_unavailable";
/// SPEC 16.1: the upstream tier is unreachable.
pub const UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";

/// Every subcode constant above, for exhaustive tests and documentation.
pub const ALL: &[&str] = &[
    NON_MINIMAL_VARINT,
    VARINT_OVERFLOW,
    LENGTH_OVERRUN,
    TRAILING_BYTES,
    UNSORTED,
    DUPLICATE,
    UNKNOWN_FORMAT_VERSION,
    UNKNOWN_CRITICAL_EXTENSION,
    INVALID_MODE,
    INVALID_KIND,
    INVALID_ALGO,
    EMPTY_TREE,
    BAD_ID_TEXT,
    MISSING_PARENT,
    DUPLICATE_PARENT,
    PARENTLESS_NOT_ALLOWED,
    DELTA_UNSORTED,
    DELTA_DUPLICATE,
    INVALID_PATH,
    MISSING_BLOB,
    SIZE_MISMATCH,
    DELETE_ABSENT,
    DELETE_NOT_FILE,
    NOOP_PUT,
    FILE_DIRECTORY,
    ROOT_TREE_MISMATCH,
    MISSING_FIELD,
    INVALID_REF_NAME,
    MULTI_PARENT,
    MERGE_IN_STACK,
    LAND_CONTENTION,
    REF_EXISTS,
    INVALID_REVISION,
    AMBIGUOUS_PREFIX,
    NOT_REDACTABLE,
    NOT_QUARANTINED,
    INJECTED,
    INJECTED_CRASH,
    TXN_RETRY,
    STORE_UNAVAILABLE,
    UPSTREAM_UNAVAILABLE,
];

/// TOO_LARGE limit names carried in [`crate::ErrorPayload::Limit`] (SPEC 6.5, PLAN 6.7).
pub mod limit_names {
    /// `limits.max_parents`.
    pub const MAX_PARENTS: &str = "max_parents";
    /// `limits.max_delta_entries`.
    pub const MAX_DELTA_ENTRIES: &str = "max_delta_entries";
    /// `limits.max_blob_bytes`.
    pub const MAX_BLOB_BYTES: &str = "max_blob_bytes";
    /// `limits.max_message_bytes`.
    pub const MAX_MESSAGE_BYTES: &str = "max_message_bytes";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn subcodes_are_unique_snake_case() {
        let set: BTreeSet<&str> = ALL.iter().copied().collect();
        assert_eq!(set.len(), ALL.len());
        for s in ALL {
            assert!(
                s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{s}"
            );
        }
    }
}
