//! Background jobs: quarantine mover and purge scheduler (SPEC 13.2; P25).
//!
//! The redaction lifecycle's background work lives in [`scion_redaction::Redactor`]: every
//! `purge_interval` it finishes pending quarantine byte moves and purges objects whose
//! quarantine period elapsed. The task holds only a weak reference to the redactor, which the
//! [`AppContext`] owns, so it stops when the server's context is dropped.

use crate::app::AppContext;
use std::sync::Arc;

/// Start the server's background jobs (SPEC 13.2): the quarantine mover and purge scheduler.
/// Outside a tokio runtime nothing is started (and a warning is logged).
pub(crate) fn spawn(ctx: &Arc<AppContext>) -> Vec<tokio::task::JoinHandle<()>> {
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::warn!("no tokio runtime; redaction background jobs not started");
        return Vec::new();
    }
    let mover = ctx.redactor.spawn_background();
    tracing::debug!(
        purge_interval_ms =
            u64::try_from(ctx.redactor.config().purge_interval.as_millis()).unwrap_or(u64::MAX),
        "redaction mover and purge scheduler started"
    );
    vec![mover]
}
