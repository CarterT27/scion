//! Derive leases: deduplication only, never required for correctness (SPEC 9.1; P14).
//!
//! A worker about to derive a lazy key tries to take the `derive_leases` row for it. When the
//! lease is held elsewhere the worker waits briefly, re-checking whether the holder has
//! written the value, and then derives anyway: values are deterministic and written with
//! `ON CONFLICT DO NOTHING`, so a duplicate derivation is harmless. Lease infrastructure
//! errors are treated as "not acquired".

use crate::ctx::read_stored;
use bytes::Bytes;
use scion_core::{Clock, Result};
use scion_store::{DerivedKey, ObjectRepo};
use std::time::Duration;
use tracing::{Instrument, debug, warn};

/// Interval between re-checks while another holder owns the lease.
const WAIT_STEP: Duration = Duration::from_millis(5);
/// Upper bound on the total wait before deriving without the lease.
const WAIT_MAX: Duration = Duration::from_millis(100);

/// A held lease (released explicitly with [`Lease::release`]).
pub(crate) struct Lease {
    key: DerivedKey,
    holder: String,
}

impl Lease {
    /// Release the lease; failures are logged and ignored (SPEC 9.1: leases only deduplicate).
    pub(crate) async fn release(self, objects: &ObjectRepo) {
        if let Err(e) = objects
            .storage()
            .meta
            .release_lease(&self.key, &self.holder)
            .await
        {
            warn!(target: "scion::lock", ty = %self.key.ty, error = %e, "derive lease release failed");
        }
    }
}

/// The outcome of [`acquire_or_wait`].
pub(crate) enum LeaseOutcome {
    /// The lease is ours: derive, write back, then release.
    Acquired(Lease),
    /// Another worker wrote the value while we waited.
    Stored(Bytes),
    /// The lease is held elsewhere (or unavailable) and no value appeared: derive anyway.
    Proceed,
}

/// Try to take the lease for `key`. If it is held by someone else, poll the stored value for a
/// short while; return [`LeaseOutcome::Proceed`] when nothing appears (SPEC 9.1).
pub(crate) async fn acquire_or_wait(
    objects: &ObjectRepo,
    clock: &dyn Clock,
    key: &DerivedKey,
    holder: &str,
    ttl: Duration,
) -> Result<LeaseOutcome> {
    let span = tracing::debug_span!(target: "scion::lock", "derive_lease", ty = %key.ty, version = key.version);
    let meta = &objects.storage().meta;
    match meta
        .try_lease(key, holder, ttl, clock.now())
        .instrument(span)
        .await
    {
        Ok(true) => {
            return Ok(LeaseOutcome::Acquired(Lease {
                key: key.clone(),
                holder: holder.to_owned(),
            }));
        }
        Ok(false) => debug!(target: "scion::lock", "derive lease held elsewhere; waiting"),
        Err(e) => {
            warn!(target: "scion::lock", error = %e, "derive lease unavailable; deriving without it");
            return Ok(LeaseOutcome::Proceed);
        }
    }
    let budget = WAIT_MAX.min(ttl);
    let mut waited = Duration::ZERO;
    while waited < budget {
        tokio::time::sleep(WAIT_STEP).await;
        waited += WAIT_STEP;
        if let Some(v) = read_stored(objects, key).await? {
            return Ok(LeaseOutcome::Stored(v));
        }
    }
    Ok(LeaseOutcome::Proceed)
}
