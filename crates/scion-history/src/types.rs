//! Request/response types of the history engine (PLAN 2.6).

use bytes::Bytes;
use scion_core::{ChangeId, EntryInfo, ObjectId};
use scion_object::{Changeset, DeltaEntry, Extensions, RefName, RepoPath, Timestamp};
use scion_store::LandedEntry;
use std::sync::Arc;

/// Client-supplied Meta fields; `creator` is always set by the server (SPEC 7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaFields {
    /// Author bytes.
    pub author: Vec<u8>,
    /// Author time.
    pub author_time: Timestamp,
    /// Committer (import only).
    pub committer: Option<Vec<u8>>,
    /// Committer time (import only).
    pub committer_time: Option<Timestamp>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Meta extensions.
    pub extensions: Extensions,
}

/// SPEC 8.1 CreateChangeset request.
#[derive(Clone, Debug)]
pub struct CreateRequest {
    /// Parent changesets.
    pub parents: Vec<ObjectId>,
    /// Delta relative to `parents[0]` (or the empty tree).
    pub delta: Vec<DeltaEntry>,
    /// Meta fields.
    pub meta: MetaFields,
    /// Change id.
    pub change_id: ChangeId,
    /// Optional expected root tree (`root_tree_mismatch`).
    pub expected_root_tree: Option<ObjectId>,
    /// Requires `WriteAuthorizer::can_import()`; allows parentless; enables git_mapping
    /// (PLAN 6.4).
    pub import_mode: bool,
}

/// SPEC 7.5 CreateChangeset response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    /// Changeset id.
    pub changeset_id: ObjectId,
    /// Root tree id.
    pub root_tree: ObjectId,
    /// Meta id.
    pub meta_id: ObjectId,
}

/// Meta of an internally created changeset (rebase reuses the original Meta).
#[derive(Clone, Debug)]
pub enum MetaSource {
    /// Build a new Meta from fields.
    New(MetaFields),
    /// Reuse an existing Meta object.
    Existing(ObjectId),
}

/// A decoded changeset with its commit-graph columns.
#[derive(Clone, Debug)]
pub struct ChangesetRecord {
    /// Changeset id.
    pub id: ObjectId,
    /// Decoded changeset.
    pub changeset: Arc<Changeset>,
    /// Generation (SPEC 9.2).
    pub generation: u64,
    /// Skip pointers (SPEC 9.2).
    pub skip: Vec<ObjectId>,
    /// Creating principal id string.
    pub created_by: String,
}

/// Result of SPEC 18.2 apply_delta.
#[derive(Clone, Debug)]
pub struct Applied {
    /// New root tree id.
    pub root: ObjectId,
    /// Every new non-empty tree with its directory path and canonical bytes.
    pub new_trees: Vec<(RepoPath, ObjectId, Bytes)>,
}

/// One file-level difference between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// File path.
    pub path: RepoPath,
    /// Entry in the old tree.
    pub old: Option<EntryInfo>,
    /// Entry in the new tree.
    pub new: Option<EntryInfo>,
}

/// SPEC 8.5 UpdateRef request.
#[derive(Clone, Debug)]
pub struct UpdateRefRequest {
    /// Ref name.
    pub name: RefName,
    /// Expected current version.
    pub expected_version: u64,
    /// New target.
    pub new_target: ObjectId,
    /// Permit a non-fast-forward update (also requires force permission).
    pub allow_non_fast_forward: bool,
}

/// SPEC 8.4 Land request.
#[derive(Clone, Debug)]
pub struct LandRequest {
    /// Target ref.
    pub ref_name: RefName,
    /// Stack head.
    pub head: ObjectId,
    /// Skip members whose rebase is EMPTY_CHANGE (PLAN 6.14).
    pub allow_empty: bool,
}

/// SPEC 8.4 Land result.
#[derive(Clone, Debug, PartialEq)]
pub struct LandResult {
    /// New ref target.
    pub target: ObjectId,
    /// New ref version.
    pub version: u64,
    /// Landed tuples.
    pub landed: Vec<LandedEntry>,
}
