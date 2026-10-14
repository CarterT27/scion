//! The daemon redaction loop (SPEC 13.3, 18.8) and periodic GetServerInfo refresh.

use super::state::{Inner, TIER};
use scion_core::{ObjectId, Result, ScionError, subcode};
use scion_proto::v1;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

impl Inner {
    async fn list_redactions(&self, token: &str, since: u64) -> Result<v1::ListRedactionsResponse> {
        let token = token.to_owned();
        self.unary(|c| {
            let token = token.clone();
            async move {
                c.with_token(Some(token))
                    .admin()
                    .list_redactions(v1::ListRedactionsRequest { since_epoch: since })
                    .await
            }
        })
        .await
    }

    /// One ListRedactions refresh (SPEC 18.8): apply the entries (re-listing from epoch 0 and
    /// replacing the set when epochs are unaccounted for, so unredactions propagate), delete
    /// cached copies, mark refreshed.
    pub async fn poll_redactions(self: &Arc<Self>) -> Result<()> {
        if self.poll_blocked.load(Ordering::Acquire) {
            return Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "redaction polling is blocked",
            ));
        }
        let _g = self.poll_lock.lock().await;
        let token = self.poll_token().ok_or_else(|| {
            ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "no valid token available for ListRedactions",
            )
        })?;
        let since = self.redactions.epoch();
        let mut resp = self.list_redactions(&token, since).await?;
        let complete = since == 0 || {
            let epochs: BTreeSet<u64> = resp
                .entries
                .iter()
                .map(|e| e.epoch)
                .filter(|e| *e > since && *e <= resp.current_epoch)
                .collect();
            resp.current_epoch >= since && epochs.len() as u64 == resp.current_epoch - since
        };
        let replace = !complete || since == 0;
        if !complete {
            resp = self.list_redactions(&token, 0).await?;
        }
        let ids: Vec<ObjectId> = resp
            .entries
            .iter()
            .filter_map(|e| ObjectId::from_bytes(&e.object_id).ok())
            .collect();
        let now = self.now();
        if replace {
            self.redactions.replace(&ids, resp.current_epoch, now);
        } else {
            self.redactions.apply(&ids, resp.current_epoch, now);
        }
        if !ids.is_empty() {
            let me = self.clone();
            let del = ids.clone();
            let removed = tokio::task::spawn_blocking(move || {
                let mut n = 0usize;
                for id in &del {
                    if me.cache.contains(id) {
                        match me.cache.remove(id) {
                            Ok(()) => n += 1,
                            Err(e) => {
                                tracing::warn!(object = %id, error = %e, "could not delete a redacted cache entry")
                            }
                        }
                    }
                }
                n
            })
            .await
            .unwrap_or(0);
            if removed > 0 {
                tracing::info!(removed, "deleted cached copies of redacted objects");
            }
        }
        self.redactions.mark_refreshed(resp.current_epoch, now);
        self.synced.store(true, Ordering::Release);
        self.sync_cache_metrics();
        self.update_redaction_metrics();
        tracing::debug!(
            epoch = self.redactions.epoch(),
            entries = ids.len(),
            "redaction set refreshed"
        );
        Ok(())
    }

    /// `scion_redaction_epoch{tier}` and `scion_redaction_lag_seconds{tier}` (SPEC 15.2).
    pub fn update_redaction_metrics(&self) {
        self.metrics
            .redaction_epoch
            .with_label_values(&[TIER])
            .set(i64::try_from(self.redactions.epoch()).unwrap_or(i64::MAX));
        let lag = self
            .now()
            .saturating_duration_since(self.redactions.last_refresh());
        self.metrics
            .redaction_lag
            .with_label_values(&[TIER])
            .set(lag.as_secs_f64());
    }

    /// Poll every `redaction_poll_interval_ms` and immediately when notified (a higher
    /// trailer epoch, SPEC 13.3); also refresh the cached GetServerInfo periodically.
    pub async fn redaction_loop(self: Arc<Self>) {
        let interval = Duration::from_millis(self.cfg.redaction_poll_interval_ms.max(1));
        let mut was_closed = false;
        loop {
            if !self.poll_blocked.load(Ordering::Acquire)
                && self.poll_token().is_some()
                && let Err(e) = self.poll_redactions().await
            {
                tracing::debug!(error = %e, "redaction poll failed");
            }
            self.update_redaction_metrics();
            let closed = self.fail_closed_now();
            if closed != was_closed {
                if closed {
                    tracing::warn!("redaction set is stale; fail-closed: forwarding every read");
                } else {
                    tracing::info!("redaction set refreshed; serving from cache again");
                }
                was_closed = closed;
            }
            let refresh_due = self
                .cached_server_info()
                .is_none_or(|(_, at)| self.now().saturating_duration_since(at) >= interval);
            if refresh_due && let Err(e) = self.refresh_server_info().await {
                tracing::debug!(error = %e, "GetServerInfo refresh failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.poll_now.notified() => {}
                _ = self.shutdown.cancelled() => return,
            }
        }
    }
}
