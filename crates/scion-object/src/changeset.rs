//! SPEC 4.3.4 Changeset.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::RepoPath;
use crate::tree::check_order;
use scion_core::{ChangeId, Kind, Limits, Mode, ObjectId, Result, ScionError, subcode};
use std::collections::BTreeSet;

/// SPEC 4.3.4 Changeset `format_version`.
const CHANGESET_FORMAT_VERSION: u64 = 1;
/// SPEC 4.3.4 DeltaEntry `op` byte for Put.
const DELTA_OP_PUT: u8 = 1;
/// SPEC 4.3.4 DeltaEntry `op` byte for Delete.
const DELTA_OP_DELETE: u8 = 2;

/// SPEC 4.3.4 delta operation: `1 Put (mode 1..=3, Blob id, size)` or `2 Delete`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaOp {
    /// Put a file-level entry.
    Put {
        /// Mode 1..=3.
        mode: Mode,
        /// Blob id.
        id: ObjectId,
        /// Blob length.
        size: u64,
    },
    /// Delete a file.
    Delete,
}

/// SPEC 4.3.4 DeltaEntry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaEntry {
    /// File path.
    pub path: RepoPath,
    /// Operation.
    pub op: DeltaOp,
}

impl DeltaEntry {
    /// Entry-local SPEC 4.3.4 rules: the path is not the root (invalid_path); a Put has
    /// mode 1..=3 (invalid_mode) and a Blob id (invalid_kind).
    pub fn validate(&self) -> Result<()> {
        if self.path.is_root() {
            return Err(ScionError::invalid(
                subcode::INVALID_PATH,
                "delta path must not be the root",
            ));
        }
        if let DeltaOp::Put { mode, id, .. } = &self.op {
            if !mode.is_file_like() {
                return Err(ScionError::invalid(
                    subcode::INVALID_MODE,
                    "delta Put mode must be 1..=3",
                ));
            }
            if id.kind() != Kind::Blob {
                return Err(ScionError::invalid(
                    subcode::INVALID_KIND,
                    format!("delta Put id must be a Blob id, got {:?}", id.kind()),
                ));
            }
        }
        Ok(())
    }

    fn encode_into(&self, e: &mut Encoder) {
        e.bytes(self.path.as_bytes());
        match &self.op {
            DeltaOp::Put { mode, id, size } => {
                e.u8(DELTA_OP_PUT);
                e.u8(*mode as u8);
                e.id(id);
                e.uvarint(*size);
            }
            DeltaOp::Delete => e.u8(DELTA_OP_DELETE),
        }
    }

    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        let path = RepoPath::parse(d.bytes()?)?;
        let op = match d.u8()? {
            DELTA_OP_PUT => {
                let mode = Mode::from_u8(d.u8()?)?;
                let id = d.id()?;
                let size = d.uvarint()?;
                DeltaOp::Put { mode, id, size }
            }
            DELTA_OP_DELETE => DeltaOp::Delete,
            other => {
                return Err(ScionError::invalid(
                    subcode::INVALID_MODE,
                    format!("invalid delta op {other}"),
                ));
            }
        };
        let entry = DeltaEntry { path, op };
        entry.validate()?;
        Ok(entry)
    }
}

/// SPEC 4.3.4 Changeset. `delta` equals `FileDiff(T1, root_tree)` relative to the first
/// parent only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Parent changesets: ordered, unique, `0..=limits.max_parents`.
    pub parents: Vec<ObjectId>,
    /// Root tree id.
    pub root_tree: ObjectId,
    /// Meta id.
    pub meta: ObjectId,
    /// Change id (16 bytes).
    pub change_id: ChangeId,
    /// Delta, strictly ascending by path.
    pub delta: Vec<DeltaEntry>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

fn expect_kind(id: &ObjectId, kind: Kind, what: &str) -> Result<()> {
    if id.kind() != kind {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!("{what} must be a {kind:?} id, got {:?}", id.kind()),
        ));
    }
    Ok(())
}

impl Canonical for Changeset {
    const KIND: Kind = Kind::Changeset;

    fn encode_into(&self, e: &mut Encoder) {
        e.uvarint(CHANGESET_FORMAT_VERSION);
        e.uvarint(self.parents.len() as u64);
        for p in &self.parents {
            e.id(p);
        }
        e.id(&self.root_tree);
        e.id(&self.meta);
        e.raw(&self.change_id.0);
        e.uvarint(self.delta.len() as u64);
        for entry in &self.delta {
            entry.encode_into(e);
        }
        self.extensions.encode_into(e);
    }

    /// Validates kinds (parents Changeset, root_tree Tree, meta Meta, Put ids Blob), parent
    /// uniqueness (duplicate), delta strictly ascending (unsorted / duplicate), paths
    /// (invalid_path), modes and extensions. Limits are not applied here; see
    /// [`Changeset::validate_shape`].
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        d.format_version(CHANGESET_FORMAT_VERSION)?;
        let n = d.list_len()?;
        let mut parents: Vec<ObjectId> = Vec::with_capacity(n);
        let mut seen = BTreeSet::new();
        for _ in 0..n {
            let p = d.id()?;
            expect_kind(&p, Kind::Changeset, "parent")?;
            if !seen.insert(p) {
                return Err(ScionError::invalid(
                    subcode::DUPLICATE,
                    format!("duplicate parent {p}"),
                ));
            }
            parents.push(p);
        }
        let root_tree = d.id()?;
        expect_kind(&root_tree, Kind::Tree, "root_tree")?;
        let meta = d.id()?;
        expect_kind(&meta, Kind::Meta, "meta")?;
        let change_id = ChangeId(d.fixed::<16>()?);
        let n = d.list_len()?;
        let mut delta: Vec<DeltaEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            let entry = DeltaEntry::decode_from(d)?;
            if let Some(prev) = delta.last() {
                check_order(prev.path.as_bytes(), entry.path.as_bytes(), "delta path")?;
            }
            delta.push(entry);
        }
        let extensions = Extensions::decode_with(d, &[])?;
        Ok(Changeset {
            parents,
            root_tree,
            meta,
            change_id,
            delta,
            extensions,
        })
    }
}

impl Changeset {
    /// Shape checks of SPEC 8.1 steps 1-2 that need no storage: parent uniqueness and count
    /// (TOO_LARGE `max_parents`), delta sorted/unique (delta_unsorted / delta_duplicate),
    /// `max_delta_entries` (TOO_LARGE) and path rules (invalid_path).
    ///
    /// Order (SPEC 8.1): parents of kind Changeset (invalid_kind), unique
    /// (duplicate_parent), count; then delta sorted, unique, count, then per entry the
    /// path rules and Put mode/kind (invalid_mode / invalid_kind). `root_tree` and `meta`
    /// are not inspected, so callers may validate before the root tree is built.
    pub fn validate_shape(&self, limits: &Limits) -> Result<()> {
        validate_parents_shape(&self.parents, limits)?;
        validate_delta_shape(&self.delta, limits)
    }
}

/// SPEC 8.1 step 1 without storage: every parent is a Changeset id (invalid_kind), parents
/// are unique (duplicate_parent) and at most `limits.max_parents` (TOO_LARGE
/// `max_parents`).
fn validate_parents_shape(parents: &[ObjectId], limits: &Limits) -> Result<()> {
    let mut seen = BTreeSet::new();
    for p in parents {
        expect_kind(p, Kind::Changeset, "parent")?;
        if !seen.insert(p) {
            return Err(ScionError::invalid(
                subcode::DUPLICATE_PARENT,
                format!("duplicate parent {p}"),
            ));
        }
    }
    let n = parents.len() as u64;
    if n > limits.max_parents {
        return Err(ScionError::too_large("max_parents", limits.max_parents, n));
    }
    Ok(())
}

/// SPEC 8.1 step 2: delta strictly ascending by path (delta_unsorted / delta_duplicate),
/// at most `limits.max_delta_entries` (TOO_LARGE `max_delta_entries`), then per entry the
/// path rules (invalid_path) and Put mode 1..=3 / Blob id (invalid_mode / invalid_kind).
fn validate_delta_shape(delta: &[DeltaEntry], limits: &Limits) -> Result<()> {
    for pair in delta.windows(2) {
        if let [a, b] = pair {
            match a.path.cmp(&b.path) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(ScionError::invalid(
                        subcode::DELTA_DUPLICATE,
                        format!("duplicate delta path {}", b.path),
                    ));
                }
                std::cmp::Ordering::Greater => {
                    return Err(ScionError::invalid(
                        subcode::DELTA_UNSORTED,
                        format!("delta path {} after {}", b.path, a.path),
                    ));
                }
            }
        }
    }
    let n = delta.len() as u64;
    if n > limits.max_delta_entries {
        return Err(ScionError::too_large(
            "max_delta_entries",
            limits.max_delta_entries,
            n,
        ));
    }
    for entry in delta {
        entry.validate()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{ErrorCode, ErrorPayload, empty_tree_id};

    fn path(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    fn put(s: &str) -> DeltaEntry {
        DeltaEntry {
            path: path(s),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: ObjectId::hash_blob(s.as_bytes()),
                size: s.len() as u64,
            },
        }
    }

    fn del(s: &str) -> DeltaEntry {
        DeltaEntry {
            path: path(s),
            op: DeltaOp::Delete,
        }
    }

    fn cs(parents: Vec<ObjectId>, delta: Vec<DeltaEntry>) -> Changeset {
        Changeset {
            parents,
            root_tree: empty_tree_id(),
            meta: ObjectId::new(Kind::Meta, [3; 32]),
            change_id: ChangeId([9; 16]),
            delta,
            extensions: Extensions::new(),
        }
    }

    fn pid(n: u8) -> ObjectId {
        ObjectId::new(Kind::Changeset, [n; 32])
    }

    #[test]
    fn round_trip() {
        let c = cs(vec![pid(1), pid(2)], vec![del("a"), put("a/b"), put("b")]);
        let b = c.encode();
        assert_eq!(Changeset::decode(&b).unwrap(), c);
        c.validate_shape(&Limits::default()).unwrap();
    }

    #[test]
    fn shape_errors() {
        let l = Limits::default();
        let sub = |c: &Changeset| {
            c.validate_shape(&l)
                .unwrap_err()
                .subcode()
                .map(str::to_owned)
        };
        assert_eq!(
            sub(&cs(vec![pid(1), pid(1)], vec![])).as_deref(),
            Some(subcode::DUPLICATE_PARENT)
        );
        assert_eq!(
            sub(&cs(vec![empty_tree_id()], vec![])).as_deref(),
            Some(subcode::INVALID_KIND)
        );
        assert_eq!(
            sub(&cs(vec![], vec![put("b"), put("a")])).as_deref(),
            Some(subcode::DELTA_UNSORTED)
        );
        assert_eq!(
            sub(&cs(vec![], vec![put("a"), del("a")])).as_deref(),
            Some(subcode::DELTA_DUPLICATE)
        );
        let root = DeltaEntry {
            path: RepoPath::root(),
            op: DeltaOp::Delete,
        };
        assert_eq!(
            sub(&cs(vec![], vec![root])).as_deref(),
            Some(subcode::INVALID_PATH)
        );
        let mut bad_mode = put("x");
        bad_mode.op = DeltaOp::Put {
            mode: Mode::Tree,
            id: empty_tree_id(),
            size: 0,
        };
        assert_eq!(
            sub(&cs(vec![], vec![bad_mode])).as_deref(),
            Some(subcode::INVALID_MODE)
        );

        let small = Limits {
            max_parents: 1,
            max_delta_entries: 1,
            ..Limits::default()
        };
        let e = cs(vec![pid(1), pid(2)], vec![])
            .validate_shape(&small)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::TooLarge);
        assert_eq!(
            e.payload,
            ErrorPayload::Limit {
                name: "max_parents".into(),
                limit: 1,
                actual: 2
            }
        );
        let e = cs(vec![], vec![put("a"), put("b")])
            .validate_shape(&small)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::TooLarge);
        assert_eq!(
            e.payload,
            ErrorPayload::Limit {
                name: "max_delta_entries".into(),
                limit: 1,
                actual: 2
            }
        );
    }
}
