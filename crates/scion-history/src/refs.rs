//! Refs, UpdateRef and the ref log (SPEC 4.3.6, 4.3.7, 5.3 step 3, 8.5; P20).
//!
//! Every ref read is strong (SPEC 5.4: `MetaStore::get_ref` / `ref_log` are always strong) and
//! every ref write is one version-checked `cas_ref` transaction that also inserts the `ref_log`
//! row (and, for admin and forced updates, the PLAN 6.8 audit row).

use crate::history::History;
use crate::hooks::LandAuthorizer;
use crate::types::UpdateRefRequest;
use scion_core::{ErrorCode, ErrorPayload, Kind, ObjectId, Principal, Result, ScionError, subcode};
use scion_object::RefName;
use scion_store::{AuditRow, CasOutcome, RefCas, RefExpect, RefLogRow, RefRow};

/// Audit action of [`History::create_ref`] (PLAN 6.8).
pub(crate) const AUDIT_REF_CREATE: &str = "ref_create";
/// Audit action of [`History::delete_ref`] (PLAN 6.8).
pub(crate) const AUDIT_REF_DELETE: &str = "ref_delete";
/// Audit action of a forced (non-fast-forward) [`History::update_ref`] (PLAN 6.8).
pub(crate) const AUDIT_REF_FORCE_UPDATE: &str = "ref_force_update";

/// REF_VERSION_MISMATCH carrying the current `(version, target)` (SPEC 8.5, 7.2).
pub(crate) fn version_mismatch(name: &RefName, expected: u64, current: &RefRow) -> ScionError {
    ScionError::new(
        ErrorCode::RefVersionMismatch,
        format!(
            "ref {} is at version {}, expected {expected}",
            name.as_str(),
            current.version
        ),
    )
    .with_payload(ErrorPayload::RefMismatch {
        current_version: current.version,
        current_target: Some(current.target),
    })
}

/// NOT_FOUND for a ref that does not exist.
pub(crate) fn ref_not_found(name: &RefName) -> ScionError {
    ScionError::not_found(format!("ref {} does not exist", name.as_str()))
}

/// A store invariant violation surfaced as UNAVAILABLE/store_unavailable (never a panic).
fn store_invariant(what: &str) -> ScionError {
    tracing::error!(what, "unexpected cas_ref outcome");
    ScionError::unavailable(
        subcode::STORE_UNAVAILABLE,
        format!("unexpected cas_ref outcome: {what}"),
    )
}

impl History {
    /// Ensure `target` names an existing changeset (NOT_FOUND / INVALID_ARGUMENT invalid_kind).
    async fn require_changeset(&self, target: &ObjectId) -> Result<()> {
        if target.kind() != Kind::Changeset {
            return Err(ScionError::invalid(
                subcode::INVALID_KIND,
                format!("ref target {target} is not a changeset id"),
            ));
        }
        self.objects.get_changeset(target).await.map(|_| ())
    }

    /// The audit row for a ref administration action (PLAN 6.8).
    fn ref_audit(
        &self,
        who: &Principal,
        action: &str,
        name: &RefName,
        reason: &str,
        detail: String,
    ) -> AuditRow {
        AuditRow {
            at: self.clock.now(),
            actor: who.id_string(),
            action: action.to_owned(),
            target: name.as_str().to_owned(),
            reason: reason.to_owned(),
            detail,
        }
    }

    /// SPEC 4.3.6, 4.3.7, 5.4: strong read of a ref.
    ///
    /// With `at_version = Some(v)` the historical target `<ref>@<v>` is served from the ref log
    /// entry with that version (its `new_target`, version `v`, `updated_at` = the entry time).
    /// NOT_FOUND when the ref (or that log entry) does not exist, or when version `v` deleted
    /// the ref.
    pub async fn resolve_ref(&self, name: &RefName, at_version: Option<u64>) -> Result<RefRow> {
        match at_version {
            None => self
                .meta()
                .get_ref(name)
                .await?
                .ok_or_else(|| ref_not_found(name)),
            Some(v) => {
                let entry = self
                    .meta()
                    .ref_log(name, v, 1)
                    .await?
                    .into_iter()
                    .find(|e| e.version == v)
                    .ok_or_else(|| {
                        ScionError::not_found(format!(
                            "ref {}@{v} is not in the ref log",
                            name.as_str()
                        ))
                    })?;
                let target = entry.new_target.ok_or_else(|| {
                    ScionError::not_found(format!("ref {}@{v} is a deletion", name.as_str()))
                })?;
                Ok(RefRow {
                    name: name.clone(),
                    target,
                    version: v,
                    updated_at: entry.at,
                })
            }
        }
    }

    /// SPEC 8.5 UpdateRef: a version-checked update with fast-forward enforcement; the `ref_log`
    /// row is written in the same transaction (SPEC 5.3 step 3).
    ///
    /// Order of checks: `update` permission (PERMISSION_DENIED); the ref exists (NOT_FOUND) at
    /// `expected_version` (REF_VERSION_MISMATCH with a `RefMismatch` payload carrying the
    /// current version and target); the new target is an existing changeset (NOT_FOUND); a
    /// non-fast-forward update (the new target is not a descendant of the current one, SPEC
    /// 18.5) requires `allow_non_fast_forward` AND `force` permission, else NOT_FAST_FORWARD.
    /// A forced update writes a PLAN 6.8 audit row in the same transaction. A concurrent update
    /// between the read and the CAS surfaces as REF_VERSION_MISMATCH (retryable).
    pub async fn update_ref(
        &self,
        req: UpdateRefRequest,
        who: &Principal,
        authz: &dyn LandAuthorizer,
    ) -> Result<RefRow> {
        let name = &req.name;
        if !authz.can_update(name) {
            return Err(ScionError::permission_denied(format!(
                "update on ref {} denied",
                name.as_str()
            )));
        }
        let current = self
            .meta()
            .get_ref(name)
            .await?
            .ok_or_else(|| ref_not_found(name))?;
        if current.version != req.expected_version {
            return Err(version_mismatch(name, req.expected_version, &current));
        }
        self.require_changeset(&req.new_target).await?;
        let fast_forward = self.is_ancestor(&current.target, &req.new_target).await?;
        let audit = if fast_forward {
            None
        } else {
            if !req.allow_non_fast_forward || !authz.can_force(name) {
                return Err(ScionError::new(
                    ErrorCode::NotFastForward,
                    format!(
                        "{} is not a descendant of {} (ref {}); a non-fast-forward update requires \
                         allow_non_fast_forward and force permission",
                        req.new_target,
                        current.target,
                        name.as_str()
                    ),
                ));
            }
            Some(self.ref_audit(
                who,
                AUDIT_REF_FORCE_UPDATE,
                name,
                "",
                format!(
                    "version {}: {} -> {}",
                    current.version, current.target, req.new_target
                ),
            ))
        };
        let outcome = self
            .meta()
            .cas_ref(RefCas {
                name: name.clone(),
                expect: RefExpect::Version(req.expected_version),
                new_target: Some(req.new_target),
                principal: who.id_string(),
                at: self.clock.now(),
                landed: Vec::new(),
                audit,
            })
            .await?;
        match outcome {
            CasOutcome::Committed(Some(row)) => {
                tracing::info!(
                    ref_name = name.as_str(),
                    version = row.version,
                    forced = !fast_forward,
                    "ref updated"
                );
                Ok(row)
            }
            CasOutcome::Committed(None) => Err(store_invariant("update committed a deletion")),
            CasOutcome::VersionMismatch { current: Some(c) } => {
                Err(version_mismatch(name, req.expected_version, &c))
            }
            CasOutcome::VersionMismatch { current: None } => Err(ref_not_found(name)),
            CasOutcome::LandedConflict { .. } => {
                Err(store_invariant("landed conflict without landed rows"))
            }
        }
    }

    /// Create a ref (INVALID_ARGUMENT/ref_exists when it exists). Its version is 1, or continues
    /// after the last logged version of a previously deleted ref (storage-semantics 4.3). The
    /// caller checks admin.refs.
    ///
    /// SPEC 4.3.6, PLAN 6.7, 6.8: one `cas_ref(Absent)` transaction writing the ref, its
    /// `ref_log` row and an audit row (`ref_create`, with `reason`). The target must be an
    /// existing changeset (NOT_FOUND).
    pub async fn create_ref(
        &self,
        name: &RefName,
        target: &ObjectId,
        who: &Principal,
        reason: &str,
    ) -> Result<RefRow> {
        self.require_changeset(target).await?;
        let audit = self.ref_audit(
            who,
            AUDIT_REF_CREATE,
            name,
            reason,
            format!("target {target}"),
        );
        let outcome = self
            .meta()
            .cas_ref(RefCas {
                name: name.clone(),
                expect: RefExpect::Absent,
                new_target: Some(*target),
                principal: who.id_string(),
                at: self.clock.now(),
                landed: Vec::new(),
                audit: Some(audit),
            })
            .await?;
        match outcome {
            CasOutcome::Committed(Some(row)) => {
                tracing::info!(
                    ref_name = name.as_str(),
                    version = row.version,
                    "ref created"
                );
                Ok(row)
            }
            CasOutcome::Committed(None) => Err(store_invariant("create committed a deletion")),
            CasOutcome::VersionMismatch { .. } => Err(ScionError::invalid(
                subcode::REF_EXISTS,
                format!("ref {} already exists", name.as_str()),
            )),
            CasOutcome::LandedConflict { .. } => {
                Err(store_invariant("landed conflict without landed rows"))
            }
        }
    }

    /// Delete a ref at `expected_version`; the caller checks admin.refs.
    ///
    /// SPEC 4.3.7, PLAN 6.8: one `cas_ref(Version(v))` transaction deleting the ref and writing
    /// its `ref_log` row (`new_target = None`) and an audit row (`ref_delete`, with `reason`).
    /// NOT_FOUND when the ref does not exist; REF_VERSION_MISMATCH (with the current version and
    /// target) when it is at another version.
    pub async fn delete_ref(
        &self,
        name: &RefName,
        expected_version: u64,
        who: &Principal,
        reason: &str,
    ) -> Result<()> {
        let audit = self.ref_audit(
            who,
            AUDIT_REF_DELETE,
            name,
            reason,
            format!("version {expected_version}"),
        );
        let outcome = self
            .meta()
            .cas_ref(RefCas {
                name: name.clone(),
                expect: RefExpect::Version(expected_version),
                new_target: None,
                principal: who.id_string(),
                at: self.clock.now(),
                landed: Vec::new(),
                audit: Some(audit),
            })
            .await?;
        match outcome {
            CasOutcome::Committed(_) => {
                tracing::info!(ref_name = name.as_str(), expected_version, "ref deleted");
                Ok(())
            }
            CasOutcome::VersionMismatch { current: Some(c) } => {
                Err(version_mismatch(name, expected_version, &c))
            }
            CasOutcome::VersionMismatch { current: None } => Err(ref_not_found(name)),
            CasOutcome::LandedConflict { .. } => {
                Err(store_invariant("landed conflict without landed rows"))
            }
        }
    }

    /// Ref log entries from `from_version`, ascending (SPEC 4.3.7; strong read).
    pub async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        self.meta().ref_log(name, from_version, limit).await
    }
}
