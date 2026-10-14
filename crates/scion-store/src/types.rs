//! Row types of the SPEC 5.2 metadata schema (plus the PLAN 6.8 audit log).

use bytes::Bytes;
use scion_core::{ChangeId, Kind, ObjectId};
use scion_object::{RefName, RepoPath};
use std::time::SystemTime;

/// `repos.repo_id` (single row in v1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RepoId(pub i64);

/// Read consistency (SPEC 5.4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Consistency {
    /// Strongly consistent read.
    Strong,
    /// Stale/follower read; a miss must be retried strongly before NOT_FOUND.
    Stale,
}

/// `objects.state` (SPEC 5.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ObjectState {
    /// Servable.
    Live,
    /// Redacted, bytes kept in quarantine (SPEC 13.2).
    Quarantined,
    /// Redacted, bytes destroyed.
    Purged,
}

/// Outcome of an ON CONFLICT DO NOTHING insert.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The row was inserted.
    Inserted,
    /// An identical-key row already existed; nothing changed.
    AlreadyExists,
}

/// Object storage area (SPEC 5.1): live `r/` or quarantine `q/`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Area {
    /// `<prefix>/r/...`.
    Live,
    /// `<prefix>/q/...`.
    Quarantine,
}

/// `objects` row (SPEC 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRow {
    /// Object id.
    pub id: ObjectId,
    /// Object kind.
    pub kind: Kind,
    /// Raw (uncompressed) size.
    pub size: u64,
    /// Stored size (compressed frame or inline length).
    pub stored_size: u64,
    /// Canonical bytes, uncompressed, for inline objects.
    pub inline_data: Option<Bytes>,
    /// Live / quarantined / purged.
    pub state: ObjectState,
    /// Insert time.
    pub created_at: SystemTime,
}

/// `object_paths` row (SPEC 5.2, 9.2 path_index).
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectPathRow {
    /// Object id.
    pub object: ObjectId,
    /// Path the object appeared at (directory path for trees).
    pub path: RepoPath,
    /// First changeset that introduced it there.
    pub first_changeset: ObjectId,
    /// Insert time.
    pub at: SystemTime,
}

/// `changesets` row (SPEC 5.2, 9.2 commit_graph columns).
#[derive(Clone, Debug, PartialEq)]
pub struct ChangesetRow {
    /// Changeset id.
    pub id: ObjectId,
    /// Canonical encoding.
    pub encoded: Bytes,
    /// Generation (root = 1).
    pub generation: u64,
    /// Skip pointers: first-parent ancestors at distance 2^k.
    pub skip: Vec<ObjectId>,
    /// Change id.
    pub change_id: ChangeId,
    /// Creating principal id string.
    pub created_by: String,
    /// Insert time.
    pub created_at: SystemTime,
}

/// Everything inserted by one `commit_changeset` transaction (SPEC 5.3 step 2).
#[derive(Clone, Debug, PartialEq)]
pub struct ChangesetBundle {
    /// The changesets row.
    pub row: ChangesetRow,
    /// Parent ids (for `changeset_children`).
    pub parents: Vec<ObjectId>,
    /// path_index rows.
    pub object_paths: Vec<ObjectPathRow>,
    /// Eager derived rows.
    pub derived: Vec<DerivedRow>,
}

/// `refs` row (SPEC 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct RefRow {
    /// Ref name.
    pub name: RefName,
    /// Target changeset.
    pub target: ObjectId,
    /// Version (starts at 1, +1 per update).
    pub version: u64,
    /// Last update time.
    pub updated_at: SystemTime,
}

/// One `(change_id, source, result)` landed tuple (SPEC 4.3.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedEntry {
    /// Change id.
    pub change_id: ChangeId,
    /// Source changeset (the stack member).
    pub source: ObjectId,
    /// Result changeset (rebased).
    pub result: ObjectId,
}

/// `ref_log` row (SPEC 4.3.7, 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct RefLogRow {
    /// Ref name.
    pub name: RefName,
    /// Version written by this update.
    pub version: u64,
    /// Previous target (`None` on create).
    pub old_target: Option<ObjectId>,
    /// New target (`None` on delete).
    pub new_target: Option<ObjectId>,
    /// Principal id string.
    pub principal: String,
    /// Update time.
    pub at: SystemTime,
    /// Landed tuples recorded by this update.
    pub landed: Vec<LandedEntry>,
}

/// `landed` row (SPEC 5.2, 8.4).
#[derive(Clone, Debug, PartialEq)]
pub struct LandedRow {
    /// Ref name.
    pub ref_name: RefName,
    /// Change id (unique per ref).
    pub change_id: ChangeId,
    /// Ref version of the landing.
    pub version: u64,
    /// Source changeset.
    pub source: ObjectId,
    /// Result changeset.
    pub result: ObjectId,
}

/// Expected ref state for a CAS.
#[derive(Clone, Debug, PartialEq)]
pub enum RefExpect {
    /// The ref must not exist (create).
    Absent,
    /// The ref must be at this version.
    Version(u64),
}

/// One version-checked ref transaction (SPEC 5.3 step 3).
#[derive(Clone, Debug, PartialEq)]
pub struct RefCas {
    /// Ref name.
    pub name: RefName,
    /// Expected state.
    pub expect: RefExpect,
    /// New target; `None` deletes the ref.
    pub new_target: Option<ObjectId>,
    /// Principal id string for the ref log.
    pub principal: String,
    /// Update time.
    pub at: SystemTime,
    /// Landed rows to insert (unique per `(ref, change_id)`).
    pub landed: Vec<LandedEntry>,
    /// Audit row written in the same transaction (PLAN 6.8).
    pub audit: Option<AuditRow>,
}

/// Outcome of [`crate::MetaStore::cas_ref`].
#[derive(Clone, Debug, PartialEq)]
pub enum CasOutcome {
    /// Committed; the new row (`None` after delete).
    Committed(Option<RefRow>),
    /// The expectation failed; nothing was written.
    VersionMismatch {
        /// Current row, if the ref exists.
        current: Option<RefRow>,
    },
    /// A landed `(ref, change_id)` already exists; the whole transaction rolled back.
    LandedConflict {
        /// The existing landed row.
        existing: LandedRow,
    },
}

/// `derived` / `derive_leases` key (SPEC 5.2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DerivedKey {
    /// Type name.
    pub ty: String,
    /// Type version.
    pub version: u32,
    /// Type-specific key.
    pub key: Bytes,
}

/// `derived` row (SPEC 5.2, 9.1).
#[derive(Clone, Debug, PartialEq)]
pub struct DerivedRow {
    /// Key.
    pub key: DerivedKey,
    /// Inline value.
    pub value: Option<Bytes>,
    /// Object storage key for large values.
    pub storage_key: Option<String>,
    /// For content-bearing records: the source object id (SPEC 9.1, 13.5).
    pub source_object: Option<ObjectId>,
    /// Insert time.
    pub at: SystemTime,
}

/// `redactions.state` (SPEC 4.3.9).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RedactionState {
    /// Bytes kept in quarantine.
    Quarantined,
    /// Bytes destroyed.
    Purged,
}

/// `redactions` row (SPEC 4.3.9, 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct RedactionRow {
    /// Redacted object.
    pub object: ObjectId,
    /// State.
    pub state: RedactionState,
    /// Reason.
    pub reason: String,
    /// Actor principal id string.
    pub actor: String,
    /// Redaction time.
    pub created_at: SystemTime,
    /// Scheduled purge time.
    pub purge_after: SystemTime,
    /// Redaction epoch stamped on this row.
    pub epoch: u64,
}

/// Input of [`crate::MetaStore::redact`] (SPEC 18.8).
#[derive(Clone, Debug, PartialEq)]
pub struct RedactTxn {
    /// Ids to quarantine.
    pub ids: Vec<ObjectId>,
    /// Reason.
    pub reason: String,
    /// Actor principal id string.
    pub actor: String,
    /// Redaction time.
    pub at: SystemTime,
    /// Scheduled purge time.
    pub purge_after: SystemTime,
}

/// `audit_log` row (PLAN 6.8; identity-free extra table allowed by SPEC 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRow {
    /// Time.
    pub at: SystemTime,
    /// Actor principal id string.
    pub actor: String,
    /// Action, e.g. `redact`, `create_ref`.
    pub action: String,
    /// Target (object id text or ref name).
    pub target: String,
    /// Reason supplied by the actor.
    pub reason: String,
    /// Free-form detail (never content or raw paths).
    pub detail: String,
}

/// Row counts per table (test support).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableCounts {
    /// `objects`.
    pub objects: usize,
    /// `changesets`.
    pub changesets: usize,
    /// `refs`.
    pub refs: usize,
    /// `ref_log`.
    pub ref_log: usize,
    /// `landed`.
    pub landed: usize,
    /// `derived`.
    pub derived: usize,
    /// `object_paths`.
    pub object_paths: usize,
    /// `redactions`.
    pub redactions: usize,
}
