//! Inversion seams of the history layer (PLAN 1.1, 2.6).

use async_trait::async_trait;
use scion_core::{ErrorCode, ObjectId, Principal, Result};
use scion_object::{Changeset, Meta, RefName, RepoPath};
use scion_store::DerivedRow;
use std::sync::Arc;
use std::time::Duration;

/// Input to eager derivation, computed before the changeset row is inserted (SPEC 5.3, 9.1).
pub struct EagerInput<'a> {
    /// The new changeset id.
    pub id: &'a ObjectId,
    /// The decoded changeset.
    pub changeset: &'a Changeset,
    /// Its Meta.
    pub meta: &'a Meta,
    /// New trees written by this changeset, with their directory paths.
    pub new_trees: &'a [(RepoPath, ObjectId)],
    /// Whether the changeset was created in import mode (PLAN 6.4).
    pub import_mode: bool,
}

/// Eager derived data hook (SPEC 9.1), implemented by `scion_derived::DerivedEngine`.
#[async_trait]
pub trait EagerHook: Send + Sync {
    /// Rows to insert in the changeset transaction.
    async fn derive_eager(&self, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>>;
    /// Eager `(type, version)` pairs every changeset must have (fsck).
    fn required_eager(&self) -> Vec<(String, u32)>;
}

/// An [`EagerHook`] producing no rows.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEager;

#[async_trait]
impl EagerHook for NoEager {
    async fn derive_eager(&self, _input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        Ok(Vec::new())
    }

    fn required_eager(&self) -> Vec<(String, u32)> {
        Vec::new()
    }
}

/// Write authorization for CreateChangeset/Merge (SPEC 8.1 step 7, 12.4).
pub trait WriteAuthorizer: Send + Sync {
    /// May the caller write `path`?
    fn can_write(&self, path: &RepoPath) -> bool;
    /// May the caller use import mode (PLAN 6.4: admin.refs and write on `/`)?
    fn can_import(&self) -> bool;
}

/// Grants everything (tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAll;

impl WriteAuthorizer for AllowAll {
    fn can_write(&self, _path: &RepoPath) -> bool {
        true
    }

    fn can_import(&self) -> bool {
        true
    }
}

/// Land/UpdateRef authorization against one policy snapshot (SPEC 8.4, 8.5).
pub trait LandAuthorizer: Send + Sync {
    /// `update` permission on the ref.
    fn can_update(&self, r: &RefName) -> bool;
    /// `force` permission on the ref.
    fn can_force(&self, r: &RefName) -> bool;
    /// `write` permission on a delta path.
    fn can_write(&self, p: &RepoPath) -> bool;
}

impl LandAuthorizer for AllowAll {
    fn can_update(&self, _r: &RefName) -> bool {
        true
    }

    fn can_force(&self, _r: &RefName) -> bool {
        true
    }

    fn can_write(&self, _p: &RepoPath) -> bool {
        true
    }
}

/// Supplies the current policy snapshot; re-read on EVERY land attempt (SPEC 8.4).
pub trait LandAuthorizerSource: Send + Sync {
    /// The current authorizer.
    fn current(&self) -> Arc<dyn LandAuthorizer>;
}

impl LandAuthorizerSource for AllowAll {
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        Arc::new(AllowAll)
    }
}

/// Rebase seam used by Land (SPEC 8.2), implemented by [`crate::History`].
#[async_trait]
pub trait Rebaser: Send + Sync {
    /// Rebase `c` onto `onto`: MERGE_CONFLICT | EMPTY_CHANGE on failure.
    async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId>;
}

/// Outcome of one land attempt (SPEC 8.4 states), reported to [`LandObserver`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandAttemptOutcome {
    /// The ref CAS committed.
    Committed,
    /// Every member was already landed (idempotent retry).
    Idempotent,
    /// The ref version changed; retrying.
    CasMismatch,
    /// A landed `(ref, change_id)` row appeared concurrently; retrying.
    LandedConflict,
    /// The land failed with this code.
    Failed(ErrorCode),
}

/// Observes land attempts (metrics: SPEC 15.2 `scion_land_attempts`, `scion_land_duration`).
pub trait LandObserver: Send + Sync {
    /// Called once per attempt.
    fn attempt(
        &self,
        ref_name: &RefName,
        attempt: u32,
        outcome: &LandAttemptOutcome,
        elapsed: Duration,
    );
}

/// A [`LandObserver`] that does nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopLandObserver;

impl LandObserver for NoopLandObserver {
    fn attempt(
        &self,
        _ref_name: &RefName,
        _attempt: u32,
        _outcome: &LandAttemptOutcome,
        _elapsed: Duration,
    ) {
    }
}
