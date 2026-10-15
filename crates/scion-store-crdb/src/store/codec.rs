//! Conversions between scion-store row types and CockroachDB column values
//! (`docs/contracts/storage-semantics.md` section 2).

use super::txn::corrupt;
use bytes::Bytes;
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result, ScionError};
use scion_object::{RefName, RepoPath};
use scion_store::{
    AuditRow, ChangesetRow, DerivedKey, DerivedRow, LandedEntry, LandedRow, ObjectRow, ObjectState,
    RedactionRow, RedactionState, RefLogRow, RefRow,
};
use std::time::SystemTime;
use tokio_postgres::Row;
use tokio_postgres::types::FromSql;

/// Bytes of one `ref_log.landed` record: `change_id (16) || source (34) || result (34)`.
const LANDED_RECORD: usize = 16 + 34 + 34;

/// A column value, mapping decode failures to a corrupt-row error.
pub(crate) fn col<'a, T: FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T> {
    row.try_get(idx)
        .map_err(|e| corrupt(format!("column {idx}: {e}")))
}

/// A `u64` as `INT8` (values above `i64::MAX` are INVALID_ARGUMENT).
pub(crate) fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("{what} {v} exceeds the storable range"),
        )
    })
}

/// A query bound (`LIMIT`, lower bounds): saturates at `i64::MAX`.
pub(crate) fn bound(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A `LIMIT` from a `usize`.
pub(crate) fn limit(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A stored non-negative `INT8` as `u64`.
pub(crate) fn to_u64(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| corrupt(format!("negative {what} {v}")))
}

/// A stored object id.
pub(crate) fn id_of(b: &[u8]) -> Result<ObjectId> {
    ObjectId::from_bytes(b).map_err(|e| corrupt(format!("object id: {e}")))
}

/// A stored optional object id.
pub(crate) fn opt_id_of(b: Option<Vec<u8>>) -> Result<Option<ObjectId>> {
    b.as_deref().map(id_of).transpose()
}

/// A stored change id.
pub(crate) fn change_id_of(b: &[u8]) -> Result<ChangeId> {
    let arr: [u8; 16] = b
        .try_into()
        .map_err(|_| corrupt(format!("change id of {} bytes", b.len())))?;
    Ok(ChangeId(arr))
}

/// A stored ref name.
pub(crate) fn ref_name_of(s: &str) -> Result<RefName> {
    RefName::parse(s).map_err(|e| corrupt(format!("ref name: {e}")))
}

/// `objects.state` text.
pub(crate) fn object_state_str(s: ObjectState) -> &'static str {
    match s {
        ObjectState::Live => "live",
        ObjectState::Quarantined => "quarantined",
        ObjectState::Purged => "purged",
    }
}

fn object_state_of(s: &str) -> Result<ObjectState> {
    match s {
        "live" => Ok(ObjectState::Live),
        "quarantined" => Ok(ObjectState::Quarantined),
        "purged" => Ok(ObjectState::Purged),
        other => Err(corrupt(format!("object state {other:?}"))),
    }
}

fn redaction_state_of(s: &str) -> Result<RedactionState> {
    match s {
        "quarantined" => Ok(RedactionState::Quarantined),
        "purged" => Ok(RedactionState::Purged),
        other => Err(corrupt(format!("redaction state {other:?}"))),
    }
}

/// The canonical `ref_log.landed` encoding: concatenated fixed-size records.
pub(crate) fn encode_landed(entries: &[LandedEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * LANDED_RECORD);
    for e in entries {
        out.extend_from_slice(&e.change_id.0);
        out.extend_from_slice(e.source.as_bytes());
        out.extend_from_slice(e.result.as_bytes());
    }
    out
}

/// Inverse of [`encode_landed`].
pub(crate) fn decode_landed(b: &[u8]) -> Result<Vec<LandedEntry>> {
    if !b.len().is_multiple_of(LANDED_RECORD) {
        return Err(corrupt(format!("ref_log.landed of {} bytes", b.len())));
    }
    b.as_chunks::<LANDED_RECORD>()
        .0
        .iter()
        .map(|c| {
            Ok(LandedEntry {
                change_id: change_id_of(&c[..16])?,
                source: id_of(&c[16..50])?,
                result: id_of(&c[50..])?,
            })
        })
        .collect()
}

/// Columns: `object_id, kind, size, stored_size, inline_data, state, created_at`.
pub(crate) fn object_row(r: &Row) -> Result<ObjectRow> {
    let kind: i16 = col(r, 1)?;
    let kind = u8::try_from(kind)
        .map_err(|_| corrupt(format!("object kind {kind}")))
        .and_then(|k| Kind::from_byte(k).map_err(|e| corrupt(format!("object kind: {e}"))))?;
    let state: String = col(r, 5)?;
    Ok(ObjectRow {
        id: id_of(&col::<Vec<u8>>(r, 0)?)?,
        kind,
        size: to_u64(col(r, 2)?, "size")?,
        stored_size: to_u64(col(r, 3)?, "stored_size")?,
        inline_data: col::<Option<Vec<u8>>>(r, 4)?.map(Bytes::from),
        state: object_state_of(&state)?,
        created_at: col(r, 6)?,
    })
}

/// Columns: `changeset_id, encoded, generation, skip, change_id, created_by, created_at,
/// fp_merges`.
pub(crate) fn changeset_row(r: &Row) -> Result<ChangesetRow> {
    let skip: Vec<Vec<u8>> = col(r, 3)?;
    Ok(ChangesetRow {
        id: id_of(&col::<Vec<u8>>(r, 0)?)?,
        encoded: Bytes::from(col::<Vec<u8>>(r, 1)?),
        generation: to_u64(col(r, 2)?, "generation")?,
        skip: skip.iter().map(|s| id_of(s)).collect::<Result<_>>()?,
        change_id: change_id_of(&col::<Vec<u8>>(r, 4)?)?,
        created_by: col(r, 5)?,
        created_at: col(r, 6)?,
        fp_merges: col::<Option<i64>>(r, 7)?
            .map(|m| to_u64(m, "fp_merges"))
            .transpose()?,
    })
}

/// Columns: `name, target, version, updated_at`.
pub(crate) fn ref_row(r: &Row) -> Result<RefRow> {
    let name: String = col(r, 0)?;
    Ok(RefRow {
        name: ref_name_of(&name)?,
        target: id_of(&col::<Vec<u8>>(r, 1)?)?,
        version: to_u64(col(r, 2)?, "ref version")?,
        updated_at: col(r, 3)?,
    })
}

/// Columns: `name, version, old_target, new_target, principal, at, landed`.
pub(crate) fn ref_log_row(r: &Row) -> Result<RefLogRow> {
    let name: String = col(r, 0)?;
    Ok(RefLogRow {
        name: ref_name_of(&name)?,
        version: to_u64(col(r, 1)?, "ref_log version")?,
        old_target: opt_id_of(col(r, 2)?)?,
        new_target: opt_id_of(col(r, 3)?)?,
        principal: col(r, 4)?,
        at: col(r, 5)?,
        landed: decode_landed(&col::<Vec<u8>>(r, 6)?)?,
    })
}

/// Columns: `ref, change_id, version, source_changeset, result_changeset`.
pub(crate) fn landed_row(r: &Row) -> Result<LandedRow> {
    let name: String = col(r, 0)?;
    Ok(LandedRow {
        ref_name: ref_name_of(&name)?,
        change_id: change_id_of(&col::<Vec<u8>>(r, 1)?)?,
        version: to_u64(col(r, 2)?, "landed version")?,
        source: id_of(&col::<Vec<u8>>(r, 3)?)?,
        result: id_of(&col::<Vec<u8>>(r, 4)?)?,
    })
}

/// Columns: `value, storage_key, source_object, at` (the key is known to the caller).
pub(crate) fn derived_row(key: &DerivedKey, r: &Row) -> Result<DerivedRow> {
    Ok(DerivedRow {
        key: key.clone(),
        value: col::<Option<Vec<u8>>>(r, 0)?.map(Bytes::from),
        storage_key: col(r, 1)?,
        source_object: opt_id_of(col(r, 2)?)?,
        at: col(r, 3)?,
    })
}

/// Columns: `object_id, state, reason, actor, created_at, purge_after, epoch`.
pub(crate) fn redaction_row(r: &Row) -> Result<RedactionRow> {
    let state: String = col(r, 1)?;
    Ok(RedactionRow {
        object: id_of(&col::<Vec<u8>>(r, 0)?)?,
        state: redaction_state_of(&state)?,
        reason: col(r, 2)?,
        actor: col(r, 3)?,
        created_at: col(r, 4)?,
        purge_after: col(r, 5)?,
        epoch: to_u64(col(r, 6)?, "redaction epoch")?,
    })
}

/// Columns: `at, actor, action, target, reason, detail`.
pub(crate) fn audit_row(r: &Row) -> Result<AuditRow> {
    Ok(AuditRow {
        at: col::<SystemTime>(r, 0)?,
        actor: col(r, 1)?,
        action: col(r, 2)?,
        target: col(r, 3)?,
        reason: col(r, 4)?,
        detail: col(r, 5)?,
    })
}

/// A stored repository path.
pub(crate) fn repo_path_of(b: &[u8]) -> Result<RepoPath> {
    RepoPath::parse(b).map_err(|e| corrupt(format!("repository path: {e}")))
}

/// Whether `p` is a valid changeset digest prefix: at most 64 lowercase hex digits.
pub(crate) fn valid_hex_prefix(p: &str) -> bool {
    p.len() <= 64
        && p.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn digest_of_padded(p: &str) -> Option<[u8; 32]> {
    let mut padded = p.to_owned();
    while padded.len() < 64 {
        padded.push('0');
    }
    let mut digest = [0u8; 32];
    hex::decode_to_slice(&padded, &mut digest).ok()?;
    Some(digest)
}

/// The primary-key byte range `[lo, hi)` of changeset ids whose digest hex starts with `p`
/// (storage-semantics 2: `01 03 || lo(p)` to `01 03 || hi(p)`; `hi` is the next prefix, or
/// the first id after every changeset id when `p` is empty or all `f`).
pub(crate) fn prefix_range(p: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let lo = ObjectId::new(Kind::Changeset, digest_of_padded(p)?)
        .as_bytes()
        .to_vec();
    // Increment `p` as a hex number of the same length.
    let mut digits: Vec<u8> = p.bytes().collect();
    let mut carry = true;
    for d in digits.iter_mut().rev() {
        if !carry {
            break;
        }
        if *d == b'f' {
            *d = b'0';
        } else {
            *d = if *d == b'9' { b'a' } else { *d + 1 };
            carry = false;
        }
    }
    let hi = if carry {
        // Empty or all-`f` prefix: everything up to the end of the changeset kind.
        let mut end = lo[..2].to_vec();
        end[1] = end[1].saturating_add(1);
        end
    } else {
        let next = String::from_utf8(digits).ok()?;
        ObjectId::new(Kind::Changeset, digest_of_padded(&next)?)
            .as_bytes()
            .to_vec()
    };
    Some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landed_round_trip() {
        let e = vec![
            LandedEntry {
                change_id: ChangeId([7; 16]),
                source: ObjectId::hash_encoded(Kind::Changeset, b"s"),
                result: ObjectId::hash_encoded(Kind::Changeset, b"r"),
            },
            LandedEntry {
                change_id: ChangeId([8; 16]),
                source: ObjectId::hash_encoded(Kind::Changeset, b"s2"),
                result: ObjectId::hash_encoded(Kind::Changeset, b"r2"),
            },
        ];
        let b = encode_landed(&e);
        assert_eq!(b.len(), 2 * LANDED_RECORD);
        assert_eq!(decode_landed(&b).unwrap(), e);
        assert!(decode_landed(&[]).unwrap().is_empty());
        assert!(decode_landed(&b[1..]).is_err());
    }

    #[test]
    fn prefix_ranges_bracket_exactly_the_matching_ids() {
        let ids: Vec<ObjectId> = (0..200u16)
            .map(|n| ObjectId::hash_encoded(Kind::Changeset, &n.to_be_bytes()))
            .collect();
        for p in [
            "", "a", "ab", "abc", "f", "ff", "0", "9", "09", "fff0", "7f",
        ] {
            let (lo, hi) = prefix_range(p).unwrap();
            assert!(lo < hi, "{p}");
            for id in &ids {
                let inside = id.as_bytes().as_slice() >= lo.as_slice()
                    && id.as_bytes().as_slice() < hi.as_slice();
                assert_eq!(inside, id.hex().starts_with(p), "prefix {p} id {id}");
            }
        }
        let full = ids[3].hex();
        let (lo, hi) = prefix_range(&full).unwrap();
        assert_eq!(lo.as_slice(), ids[3].as_bytes().as_slice());
        assert!(hi.as_slice() > lo.as_slice());
        let all_f = "f".repeat(64);
        let (lo, hi) = prefix_range(&all_f).unwrap();
        assert!(lo < hi);
        assert_eq!(hi, vec![0x01, 0x04]);
    }

    #[test]
    fn hex_prefix_validation() {
        assert!(valid_hex_prefix(""));
        assert!(valid_hex_prefix("0123456789abcdef"));
        assert!(!valid_hex_prefix("ABCD"));
        assert!(!valid_hex_prefix("xyz"));
        assert!(!valid_hex_prefix(&"0".repeat(65)));
    }

    #[test]
    fn integer_conversions() {
        assert_eq!(to_i64(5, "x").unwrap(), 5);
        assert_eq!(
            to_i64(u64::MAX, "x").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(bound(u64::MAX), i64::MAX);
        assert_eq!(limit(usize::MAX), i64::MAX);
        assert!(to_u64(-1, "x").is_err());
        assert_eq!(to_u64(9, "x").unwrap(), 9);
    }

    #[test]
    fn state_strings_round_trip() {
        for s in [
            ObjectState::Live,
            ObjectState::Quarantined,
            ObjectState::Purged,
        ] {
            assert_eq!(object_state_of(object_state_str(s)).unwrap(), s);
        }
        assert!(object_state_of("bogus").is_err());
        assert!(redaction_state_of("live").is_err());
    }
}
