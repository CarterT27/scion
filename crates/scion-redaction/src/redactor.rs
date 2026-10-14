//! The redaction lifecycle (SPEC 13.2, 18.8; PLAN 2.8; P22).
//!
//! ```text
//! Live --Redact--> Quarantined --(purge_after elapsed | PurgeNow)--> Purged
//!                      |
//!                      +--Unredact--> Live
//! ```
//!
//! State transitions are single [`MetaStore`](scion_store::MetaStore) transactions (which also
//! write the SPEC 17 audit rows); byte moves happen outside them and are idempotent, so a crash
//! at any point is completed by re-running the operation (or by the background mover).
//! Authorization (`admin.redact`, `admin.purge`) is checked by the server, not here.

use crate::config::{RedactOutcome, RedactionConfig};
use scion_core::{
    Clock, ErrorCode, Kind, ObjectId, Principal, Result, ScionError, points, subcode,
};
use scion_derived::DerivedEngine;
use scion_store::{
    Area, AuditRow, Consistency, ObjectRepo, ObjectRow, ObjectState, RedactTxn, RedactionRow,
    RedactionState,
};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// The principal recorded in the audit log for scheduled purges (SPEC 13.2 "the scheduled
/// job"; PLAN 6.5 service principal).
pub const SCHEDULER_ACTOR: &str = "service:scion";

/// The audit reason of a scheduled purge (SPEC 13.2, 17).
pub const SCHEDULED_PURGE_REASON: &str = "scheduled purge: quarantine period elapsed";

/// The audit reason of [`Redactor::unredact`], whose RPC carries no reason (SPEC 7.7, 17).
pub const DEFAULT_UNREDACT_REASON: &str = "unredact";

/// How many due ids one `due_purges` call fetches.
const PURGE_BATCH: usize = 256;

/// The shortest scheduler period (a zero `purge_interval` would make `tokio::time::interval`
/// panic).
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// Executes redaction state transitions and byte moves (SPEC 13.2).
pub struct Redactor {
    objects: ObjectRepo,
    derived: Arc<DerivedEngine>,
    clock: Arc<dyn Clock>,
    cfg: RedactionConfig,
    /// Serializes transitions and byte moves inside this process, so the background mover can
    /// never move bytes of an object that a concurrent unredact is restoring. The guarded state
    /// remembers which quarantined objects the mover already confirmed moved.
    lock: Mutex<MoverState>,
}

/// State guarded by [`Redactor::lock`].
#[derive(Default)]
struct MoverState {
    /// Quarantined ids whose live key the mover has confirmed absent, with the redaction epoch
    /// of that check. The mover skips them while their redaction row keeps that epoch, so a
    /// steady-state run costs one metadata read instead of one object-storage HEAD per
    /// quarantined object. Entries are dropped whenever this process may put bytes back at the
    /// live key (unredact) and when the id leaves the quarantined set.
    confirmed: HashMap<ObjectId, u64>,
}

impl Redactor {
    /// Assemble the redactor (no IO). SPEC 13.2.
    pub fn new(
        objects: ObjectRepo,
        derived: Arc<DerivedEngine>,
        clock: Arc<dyn Clock>,
        cfg: RedactionConfig,
    ) -> Arc<Self> {
        Arc::new(Redactor {
            objects,
            derived,
            clock,
            cfg,
            lock: Mutex::new(MoverState::default()),
        })
    }

    /// The configuration (SPEC 6.1 `[redaction]`).
    pub fn config(&self) -> &RedactionConfig {
        &self.cfg
    }

    /// SPEC 13.2 step 1-2 / 18.8 Redact: quarantine Blob/Meta ids (INVALID_ARGUMENT/
    /// not_redactable otherwise; NOT_FOUND for unknown ids) in one transaction that bumps the
    /// redaction epoch, stamps the rows and writes one audit row per id; then move the bytes to
    /// the quarantine key outside the transaction. Returns `{epoch, effective_by = commit time +
    /// propagation_bound}` (SPEC 13.3).
    ///
    /// A failed byte move does not fail the call (the objects are already served to no one);
    /// [`Redactor::move_pending_bytes`] completes it.
    pub async fn redact(
        &self,
        ids: &[ObjectId],
        reason: &str,
        actor: &Principal,
        purge_after: Option<Duration>,
    ) -> Result<RedactOutcome> {
        let ids = normalize(ids)?;
        if let Some(id) = ids
            .iter()
            .find(|id| !matches!(id.kind(), Kind::Blob | Kind::Meta))
        {
            return Err(ScionError::invalid(
                subcode::NOT_REDACTABLE,
                format!("object {id} is not redactable: only blobs and metas are (SPEC 13.1)"),
            ));
        }
        let rows = self.objects.stat_many(&ids).await?;
        let mut found = Vec::with_capacity(ids.len());
        for (id, row) in ids.iter().zip(rows) {
            match row {
                Some(r) if matches!(r.kind, Kind::Blob | Kind::Meta) => found.push(r),
                Some(_) => {
                    return Err(ScionError::invalid(
                        subcode::NOT_REDACTABLE,
                        format!("object {id} is not redactable"),
                    ));
                }
                None => return Err(ScionError::not_found(format!("object {id} not found"))),
            }
        }

        let _guard = self.lock.lock().await;
        let now = self.clock.now();
        let quarantine = purge_after.unwrap_or(self.cfg.default_quarantine);
        let purge_at = add(now, quarantine, "purge_after")?;
        let effective_by = add(now, self.cfg.propagation_bound, "propagation_bound")?;
        let epoch = self
            .objects
            .storage()
            .meta
            .redact(RedactTxn {
                ids: ids.clone(),
                reason: reason.to_owned(),
                actor: actor.id_string(),
                at: now,
                purge_after: purge_at,
            })
            .await?;
        for id in &ids {
            self.objects.evict_cached(id);
        }
        info!(
            event = "redact",
            epoch,
            objects = ids.len(),
            actor = %actor,
            "redaction committed"
        );
        self.objects
            .storage()
            .faults
            .check(points::REDACT_AFTER_COMMIT)
            .await?;

        for row in found.iter().filter(|r| has_stored_bytes(r)) {
            if let Err(e) = self.objects.quarantine_bytes(&row.id).await {
                warn!(
                    event = "quarantine_move_deferred",
                    object = %row.id,
                    error = %e,
                    "byte move to quarantine failed; the background mover will finish it"
                );
            }
        }
        Ok(RedactOutcome {
            epoch,
            effective_by,
        })
    }

    /// SPEC 13.2 step 3 Unredact: restore quarantined ids (INVALID_ARGUMENT/not_quarantined
    /// otherwise, including after purge per PLAN 6.7); returns the new epoch. The audit reason
    /// is [`DEFAULT_UNREDACT_REASON`].
    pub async fn unredact(&self, ids: &[ObjectId], actor: &Principal) -> Result<u64> {
        self.unredact_with_reason(ids, actor, DEFAULT_UNREDACT_REASON)
            .await
    }

    /// [`Redactor::unredact`] recording `reason` in the audit row (SPEC 13.2, 17). The bytes
    /// are restored first, then one `MetaStore::unredact` transaction bumps the epoch, deletes
    /// the redaction rows, sets the objects live and appends the audit row. If that transaction
    /// fails, the bytes are moved back to quarantine (best effort; the mover finishes it).
    ///
    /// An object whose purge already deleted its bytes (a purge interrupted after
    /// `purge.after_bytes`) is INVALID_ARGUMENT/not_quarantined: purge is irreversible.
    pub async fn unredact_with_reason(
        &self,
        ids: &[ObjectId],
        actor: &Principal,
        reason: &str,
    ) -> Result<u64> {
        let ids = normalize(ids)?;
        let mut state = self.lock.lock().await;
        let rows = self.strong_rows(&ids).await?;
        for (id, row) in ids.iter().zip(&rows) {
            if !matches!(row, Some(r) if r.state == ObjectState::Quarantined) {
                return Err(ScionError::invalid(
                    subcode::NOT_QUARANTINED,
                    format!("object {id} is not quarantined"),
                ));
            }
        }
        let stored: Vec<ObjectId> = rows
            .iter()
            .flatten()
            .filter(|r| has_stored_bytes(r))
            .map(|r| r.id)
            .collect();
        // A purge that crashed after deleting the bytes (checkpoint `purge.after_bytes`) leaves
        // the object quarantined with no bytes anywhere. Purge is irreversible (SPEC 13.2 step
        // 4): such an object must not become live again (a live row implies durable bytes), so
        // it is not quarantined for unredact purposes. Checked before any side effect.
        let blobs = &self.objects.storage().blobs;
        for id in &stored {
            let q = blobs
                .exists(&self.objects.key(id, Area::Quarantine))
                .await?;
            if !q && !blobs.exists(&self.objects.key(id, Area::Live)).await? {
                return Err(ScionError::invalid(
                    subcode::NOT_QUARANTINED,
                    format!("object {id} is being purged; its bytes are gone"),
                ));
            }
        }
        for id in &ids {
            state.confirmed.remove(id);
        }
        for (i, id) in stored.iter().enumerate() {
            if let Err(e) = self.objects.restore_bytes(id).await {
                self.requarantine(&stored[..i]).await;
                return Err(e);
            }
        }
        let audit = AuditRow {
            at: self.clock.now(),
            actor: actor.id_string(),
            action: "unredact".to_owned(),
            target: join_ids(&ids),
            reason: reason.to_owned(),
            detail: format!("objects={}", ids.len()),
        };
        match self.objects.storage().meta.unredact(&ids, audit).await {
            Ok(epoch) => {
                for id in &ids {
                    self.objects.evict_cached(id);
                }
                info!(event = "unredact", epoch, objects = ids.len(), actor = %actor, "unredacted");
                Ok(epoch)
            }
            Err(e) => {
                self.requarantine(&stored).await;
                Err(e)
            }
        }
    }

    /// Move restored bytes back to quarantine after a failed unredact (best effort; the mover
    /// finishes whatever fails here).
    async fn requarantine(&self, ids: &[ObjectId]) {
        for id in ids {
            if let Err(e) = self.objects.quarantine_bytes(id).await {
                warn!(
                    event = "quarantine_move_deferred",
                    object = %id,
                    error = %e,
                    "re-quarantine after a failed unredact failed; the mover will finish it"
                );
            }
        }
    }

    /// SPEC 13.2 step 4 PurgeNow: irreversibly purge quarantined ids now. Deletes the live and
    /// quarantine keys, content-bearing derived records keyed by the id (SPEC 13.5), and the
    /// inline data, and marks the object purged with an audit row. Unknown ids are NOT_FOUND,
    /// live ids INVALID_ARGUMENT/not_quarantined; already purged ids are skipped. A crash part
    /// way (checkpoint `purge.after_bytes`) leaves the object quarantined, and a re-run
    /// completes it.
    pub async fn purge_now(&self, ids: &[ObjectId], actor: &Principal, reason: &str) -> Result<()> {
        let ids = normalize(ids)?;
        let _guard = self.lock.lock().await;
        let rows = self.strong_rows(&ids).await?;
        let mut todo = Vec::with_capacity(ids.len());
        for (id, row) in ids.iter().zip(rows) {
            match row.map(|r| r.state) {
                None => return Err(ScionError::not_found(format!("object {id} not found"))),
                Some(ObjectState::Live) => {
                    return Err(ScionError::invalid(
                        subcode::NOT_QUARANTINED,
                        format!("object {id} is not quarantined"),
                    ));
                }
                Some(ObjectState::Purged) => {}
                Some(ObjectState::Quarantined) => todo.push(*id),
            }
        }
        let actor = actor.id_string();
        for id in &todo {
            self.purge_one(id, &actor, reason).await?;
        }
        Ok(())
    }

    /// SPEC 13.2 scheduled purge: purge every quarantined id whose `purge_after` has passed
    /// (`due_purges(now)`); returns how many were purged.
    ///
    /// One failing id does not block the others in its batch: every id of the batch is
    /// attempted, then the first error is returned (and no further batch is fetched, since the
    /// failed ids would head it again). The next run retries them.
    pub async fn run_due_purges(&self) -> Result<usize> {
        let _guard = self.lock.lock().await;
        let now = self.clock.now();
        let mut purged = 0;
        let mut first_err: Option<ScionError> = None;
        loop {
            let due = self
                .objects
                .storage()
                .meta
                .due_purges(now, PURGE_BATCH)
                .await?;
            for id in &due {
                match self
                    .purge_one(id, SCHEDULER_ACTOR, SCHEDULED_PURGE_REASON)
                    .await
                {
                    Ok(()) => purged += 1,
                    Err(e) => {
                        warn!(event = "scheduled_purge_failed", object = %id, error = %e, "scheduled purge failed; will retry");
                        first_err.get_or_insert(e);
                    }
                }
            }
            if due.len() < PURGE_BATCH || first_err.is_some() {
                break;
            }
        }
        if purged > 0 {
            info!(
                event = "scheduled_purge",
                objects = purged,
                "purged due redactions"
            );
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(purged),
        }
    }

    /// SPEC 13.2 step 2: finish pending quarantine copies (idempotent, resumable after a crash
    /// at `quarantine.after_copy` or before the move started); returns how many were moved.
    /// Every quarantined object whose live key still exists is moved; inline objects have no
    /// bytes to move (their inline data is never served). Objects this process already
    /// confirmed moved (at the same redaction epoch) are not re-checked, so a steady-state run
    /// is one metadata read; the first run after start checks every quarantined object, which
    /// covers moves left unfinished by a crashed process.
    pub async fn move_pending_bytes(&self) -> Result<usize> {
        let mut state = self.lock.lock().await;
        let meta = &self.objects.storage().meta;
        let (redactions, _) = meta.list_redactions(0).await?;
        let quarantined: HashMap<ObjectId, u64> = redactions
            .iter()
            .filter(|r| r.state == RedactionState::Quarantined)
            .map(|r| (r.object, r.epoch))
            .collect();
        // Forget ids that left the quarantined set (purged or unredacted).
        state
            .confirmed
            .retain(|id, epoch| quarantined.get(id) == Some(epoch));
        let mut pending: Vec<ObjectId> = quarantined
            .keys()
            .filter(|id| !state.confirmed.contains_key(id))
            .copied()
            .collect();
        if pending.is_empty() {
            return Ok(0);
        }
        pending.sort();
        let rows = meta.get_objects(&pending, Consistency::Strong).await?;
        let blobs = &self.objects.storage().blobs;
        let mut moved = 0;
        let mut first_err: Option<ScionError> = None;
        for row in rows.iter().flatten() {
            if row.state != ObjectState::Quarantined {
                continue;
            }
            let Some(&epoch) = quarantined.get(&row.id) else {
                continue;
            };
            if !has_stored_bytes(row) {
                // Inline: nothing to move, ever.
                state.confirmed.insert(row.id, epoch);
                continue;
            }
            let step = async {
                if blobs.exists(&self.objects.key(&row.id, Area::Live)).await? {
                    self.objects.quarantine_bytes(&row.id).await?;
                    Ok::<bool, ScionError>(true)
                } else {
                    Ok(false)
                }
            };
            match step.await {
                Ok(was_pending) => {
                    if was_pending {
                        debug!(object = %row.id, "moved bytes to quarantine");
                        moved += 1;
                    }
                    state.confirmed.insert(row.id, epoch);
                }
                Err(e) => {
                    warn!(object = %row.id, error = %e, "quarantine move failed");
                    first_err.get_or_insert(e);
                }
            }
        }
        if moved > 0 {
            info!(
                event = "quarantine_moves",
                objects = moved,
                "finished pending quarantine moves"
            );
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(moved),
        }
    }

    /// SPEC 13.3 / 7.7 ListRedactions: redactions with epoch greater than `since_epoch`, plus
    /// the current epoch.
    pub async fn list(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        self.objects
            .storage()
            .meta
            .list_redactions(since_epoch)
            .await
    }

    /// Spawn the quarantine mover and purge scheduler (SPEC 13.2): every `purge_interval` it
    /// runs [`Redactor::move_pending_bytes`] and then [`Redactor::run_due_purges`], logging
    /// failures. The task ends when the last `Arc<Redactor>` is dropped (or when aborted).
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(self);
        let period = self.cfg.purge_interval.max(MIN_INTERVAL);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(me) = weak.upgrade() else {
                    debug!("redactor dropped; stopping the background mover");
                    break;
                };
                if let Err(e) = me.move_pending_bytes().await {
                    warn!(event = "redaction_mover_failed", error = %e, "quarantine mover run failed");
                }
                if let Err(e) = me.run_due_purges().await {
                    warn!(event = "purge_scheduler_failed", error = %e, "scheduled purge run failed");
                }
            }
        })
    }

    /// Purge one quarantined id: bytes (live and quarantine keys), content-bearing derived
    /// records, checkpoint `purge.after_bytes`, then `mark_purged` (clears inline data) with
    /// its audit row. Every step is idempotent.
    async fn purge_one(&self, id: &ObjectId, actor: &str, reason: &str) -> Result<()> {
        self.objects.purge_bytes(id).await?;
        let derived = self.derived.purge_content_for(id).await?;
        self.objects
            .storage()
            .faults
            .check(points::PURGE_AFTER_BYTES)
            .await?;
        let audit = AuditRow {
            at: self.clock.now(),
            actor: actor.to_owned(),
            action: "purge".to_owned(),
            target: id.to_text(),
            reason: reason.to_owned(),
            detail: format!("derived_records_deleted={derived}"),
        };
        self.objects
            .storage()
            .meta
            .mark_purged(std::slice::from_ref(id), audit)
            .await?;
        self.objects.evict_cached(id);
        info!(event = "purge", object = %id, actor, derived_deleted = derived, "object purged");
        Ok(())
    }

    /// Strong objects rows (redaction state must never come from a stale read; SPEC 5.4).
    async fn strong_rows(&self, ids: &[ObjectId]) -> Result<Vec<Option<ObjectRow>>> {
        self.objects
            .storage()
            .meta
            .get_objects(ids, Consistency::Strong)
            .await
    }
}

/// Sorted, de-duplicated ids; an empty request is INVALID_ARGUMENT/missing_field.
fn normalize(ids: &[ObjectId]) -> Result<Vec<ObjectId>> {
    if ids.is_empty() {
        return Err(ScionError::invalid(
            subcode::MISSING_FIELD,
            "at least one object id is required",
        ));
    }
    let mut v = ids.to_vec();
    v.sort();
    v.dedup();
    Ok(v)
}

/// True when the object's bytes live in object storage (not inline in its row).
fn has_stored_bytes(row: &ObjectRow) -> bool {
    row.inline_data.is_none()
}

fn add(t: SystemTime, d: Duration, what: &str) -> Result<SystemTime> {
    t.checked_add(d).ok_or_else(|| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("{what} overflows the time range"),
        )
    })
}

fn join_ids(ids: &[ObjectId]) -> String {
    ids.iter()
        .map(ObjectId::to_text)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_sorts_dedups_and_rejects_empty() {
        let a = ObjectId::hash_blob(b"a");
        let b = ObjectId::hash_blob(b"b");
        let v = normalize(&[b, a, b]).unwrap();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(v, want);
        let e = normalize(&[]).unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD));
    }

    #[test]
    fn join_ids_uses_text_form() {
        let a = ObjectId::hash_blob(b"a");
        let b = ObjectId::hash_blob(b"b");
        assert_eq!(
            join_ids(&[a, b]),
            format!("{},{}", a.to_text(), b.to_text())
        );
    }

    #[test]
    fn add_overflow_is_invalid_argument() {
        let e = add(SystemTime::UNIX_EPOCH, Duration::MAX, "purge_after").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }
}
