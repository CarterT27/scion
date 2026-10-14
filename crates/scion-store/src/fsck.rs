//! SPEC 4.5 invariant oracle used by every crash test.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::repo::ObjectRepo;
use scion_core::{ObjectId, Result};

/// fsck options.
#[derive(Clone, Debug)]
pub struct FsckOptions {
    /// Eager derived types `(name, version)` every changeset must have.
    pub required_eager: Vec<(String, u32)>,
    /// Also verify the digest of every live object's bytes.
    pub verify_bytes: bool,
}

/// One SPEC 4.5 invariant violation.
#[derive(Clone, Debug, PartialEq)]
pub enum Violation {
    /// A changeset references an object that does not exist.
    MissingObject {
        /// The changeset.
        changeset: ObjectId,
        /// The missing object.
        object: ObjectId,
        /// `root_tree`, `meta`, `parent`, `delta_blob` or `tree`.
        role: &'static str,
    },
    /// Stored bytes do not hash to the id.
    DigestMismatch {
        /// The object.
        id: ObjectId,
    },
    /// Stored bytes do not decode canonically.
    BadEncoding {
        /// The object.
        id: ObjectId,
    },
    /// `delta != FileDiff(parent tree, root tree)`.
    DeltaMismatch {
        /// The changeset.
        changeset: ObjectId,
    },
    /// A required eager derived row is missing.
    MissingEager {
        /// The changeset.
        changeset: ObjectId,
        /// The type name.
        ty: String,
    },
    /// Generation or skip pointers are inconsistent.
    BadGraph {
        /// The changeset.
        changeset: ObjectId,
    },
    /// A ref points at a missing changeset.
    RefTargetMissing {
        /// The ref name.
        name: String,
    },
}

/// Walk every changeset and ref and report SPEC 4.5 violations. Quarantined or purged bytes
/// are not violations.
pub async fn fsck(objects: &ObjectRepo, opts: &FsckOptions) -> Result<Vec<Violation>> {
    todo!("P03")
}
