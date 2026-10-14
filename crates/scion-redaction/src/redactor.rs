//! The redaction lifecycle (SPEC 13.2; P22).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::config::{RedactOutcome, RedactionConfig};
use scion_core::{Clock, ObjectId, Principal, Result};
use scion_derived::DerivedEngine;
use scion_store::{ObjectRepo, RedactionRow};
use std::sync::Arc;
use std::time::Duration;

/// Executes redaction state transitions and byte moves.
pub struct Redactor {
    objects: ObjectRepo,
    derived: Arc<DerivedEngine>,
    clock: Arc<dyn Clock>,
    cfg: RedactionConfig,
}

impl Redactor {
    /// Assemble the redactor (no IO).
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
        })
    }

    /// Quarantine Blob/Meta ids (INVALID_ARGUMENT/not_redactable otherwise; NOT_FOUND for
    /// unknown ids) in one transaction with audit, then move bytes asynchronously.
    pub async fn redact(
        &self,
        ids: &[ObjectId],
        reason: &str,
        actor: &Principal,
        purge_after: Option<Duration>,
    ) -> Result<RedactOutcome> {
        todo!("P22")
    }

    /// Restore quarantined ids (INVALID_ARGUMENT/not_quarantined otherwise); returns the new
    /// epoch.
    pub async fn unredact(&self, ids: &[ObjectId], actor: &Principal) -> Result<u64> {
        todo!("P22")
    }

    /// Irreversibly purge ids now (bytes, inline data, content-bearing derived records).
    pub async fn purge_now(&self, ids: &[ObjectId], actor: &Principal, reason: &str) -> Result<()> {
        todo!("P22")
    }

    /// Purge every quarantined id whose `purge_after` has passed; returns how many.
    pub async fn run_due_purges(&self) -> Result<usize> {
        todo!("P22")
    }

    /// Finish pending quarantine copies (idempotent, resumable); returns how many moved.
    pub async fn move_pending_bytes(&self) -> Result<usize> {
        todo!("P22")
    }

    /// Redactions with epoch greater than `since_epoch`, plus the current epoch.
    pub async fn list(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        todo!("P22")
    }

    /// Spawn the quarantine mover and purge scheduler.
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        todo!("P22")
    }
}
