//! SPEC 9.2 `git_mapping` (eager, import mode only; SPEC 14; implementation-defined 6.4; P21).
//!
//! When a changeset is created with `import_mode` and its Meta carries extension tag 2
//! (`META_EXT_GIT_COMMIT`, the 20-byte git commit id, SPEC 4.3.3), two rows are emitted:
//!
//! - key `g:<40 lowercase hex>` -> value = the 34 raw changeset id bytes;
//! - key `c:<34 raw changeset id bytes>` -> value = the 40 lowercase hex digits (ASCII).
//!
//! A changeset created without `import_mode` never creates mappings, even when its Meta
//! carries tag 2. Because every derived row is inserted `ON CONFLICT DO NOTHING`, re-committing
//! an existing changeset in import mode fills in mapping rows that are missing.

use crate::engine::DerivedEngine;
use crate::types::{DeriveCtx, DeriveMode, DerivedType, EagerType};
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_history::EagerInput;
use scion_object::META_EXT_GIT_COMMIT;
use scion_store::{DerivedKey, DerivedRow};
use std::time::SystemTime;

/// Type name.
pub const NAME: &str = "git_mapping";
/// Type version.
pub const VERSION: u32 = 1;

/// Length of a git commit id in bytes (SHA-1).
const GIT_ID_LEN: usize = 20;

/// Emits `g:<40hex> -> changeset` and `c:<changeset id bytes> -> 40hex` when the changeset is
/// created in import mode and its Meta carries extension tag 2 (SPEC 14, 9.2).
#[derive(Clone, Copy, Debug, Default)]
pub struct GitMappingType;

impl DerivedType for GitMappingType {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn mode(&self) -> DeriveMode {
        DeriveMode::Eager
    }

    fn content_bearing(&self) -> bool {
        false
    }
}

/// The row key `g:<40hex>` of git commit `sha_hex` (already normalized to lowercase).
pub fn git_key(sha_hex: &str) -> Bytes {
    let mut k = Vec::with_capacity(2 + sha_hex.len());
    k.extend_from_slice(b"g:");
    k.extend_from_slice(sha_hex.as_bytes());
    Bytes::from(k)
}

/// The row key `c:<34 raw id bytes>` of changeset `cs`.
pub fn changeset_key(cs: &ObjectId) -> Bytes {
    let mut k = Vec::with_capacity(2 + 34);
    k.extend_from_slice(b"c:");
    k.extend_from_slice(cs.as_bytes());
    Bytes::from(k)
}

/// A git_mapping row (eager rows carry no wall-clock time; SPEC 9.1 determinism).
fn row(key: Bytes, value: Bytes) -> DerivedRow {
    DerivedRow {
        key: DerivedKey {
            ty: NAME.to_owned(),
            version: VERSION,
            key,
        },
        value: Some(value),
        storage_key: None,
        source_object: None,
        at: SystemTime::UNIX_EPOCH,
    }
}

#[async_trait]
impl EagerType for GitMappingType {
    /// Both mapping rows for an import-mode changeset whose Meta has a 20-byte tag 2; no rows
    /// otherwise (SPEC 14; implementation-defined 6.4).
    async fn derive(&self, _cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        if !input.import_mode {
            return Ok(Vec::new());
        }
        let Some(sha) = input.meta.extensions.get(META_EXT_GIT_COMMIT) else {
            return Ok(Vec::new());
        };
        if sha.len() != GIT_ID_LEN {
            tracing::warn!(
                changeset = %input.id,
                len = sha.len(),
                "Meta extension tag 2 is not a 20-byte git commit id; no git_mapping rows"
            );
            return Ok(Vec::new());
        }
        let hex = hex::encode(sha);
        Ok(vec![
            row(git_key(&hex), Bytes::copy_from_slice(input.id.as_bytes())),
            row(changeset_key(input.id), Bytes::from(hex.into_bytes())),
        ])
    }
}

/// Normalize a 40-hex git commit id (case-insensitive) to lowercase; INVALID_ARGUMENT /
/// bad_id_text otherwise.
fn normalize_sha(sha_hex: &str) -> Result<String> {
    if sha_hex.len() != 2 * GIT_ID_LEN || !sha_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ScionError::invalid(
            subcode::BAD_ID_TEXT,
            "a git commit id is 40 hex digits",
        ));
    }
    Ok(sha_hex.to_ascii_lowercase())
}

/// The changeset imported from git commit `sha_hex` (40 hex, case-insensitive; SPEC 14, 11.8
/// `git:<sha>` revisions). `None` when no mapping is stored; INTEGRITY when the stored value is
/// not a changeset id.
pub async fn changeset_for_git(e: &DerivedEngine, sha_hex: &str) -> Result<Option<ObjectId>> {
    let sha = normalize_sha(sha_hex)?;
    match e.get_stored(NAME, Some(VERSION), &git_key(&sha)).await? {
        Some(v) => {
            let id = ObjectId::from_bytes(&v)?;
            if id.kind() != Kind::Changeset {
                return Err(ScionError::new(
                    ErrorCode::Integrity,
                    format!("git_mapping value for {sha} is not a changeset id"),
                ));
            }
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

/// The git commit (40 lowercase hex) a changeset was imported from (SPEC 14). `None` when no
/// mapping is stored; INTEGRITY when the stored value is not 40 lowercase hex digits.
pub async fn git_for_changeset(e: &DerivedEngine, cs: &ObjectId) -> Result<Option<String>> {
    match e
        .get_stored(NAME, Some(VERSION), &changeset_key(cs))
        .await?
    {
        Some(v) => {
            if v.len() != 2 * GIT_ID_LEN
                || !v
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            {
                return Err(ScionError::new(
                    ErrorCode::Integrity,
                    format!("git_mapping value for {cs} is not 40 lowercase hex digits"),
                ));
            }
            Ok(Some(v.iter().map(|&b| char::from(b)).collect()))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_normalization() {
        let sha = "AB".repeat(20);
        assert_eq!(normalize_sha(&sha).unwrap(), "ab".repeat(20));
        assert_eq!(&git_key("00")[..], b"g:00");
        let cs = ObjectId::new(scion_core::Kind::Changeset, [3; 32]);
        let k = changeset_key(&cs);
        assert_eq!(&k[..2], b"c:");
        assert_eq!(&k[2..], cs.as_bytes());
        for bad in ["", "ab", &"g".repeat(40), &"a".repeat(41)] {
            let e = normalize_sha(bad).unwrap_err();
            assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT));
        }
    }
}
