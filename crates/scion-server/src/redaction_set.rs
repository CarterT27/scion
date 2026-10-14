//! In-memory redaction set refreshed within the propagation bound (SPEC 13.3; P23).
//!
//! The set mirrors the `redactions` table (quarantined and purged ids) together with the
//! redaction epoch it reflects. A refresh reads `list_redactions(since = current epoch)` in one
//! metadata transaction and applies the rows incrementally when every epoch increment since the
//! last refresh is accounted for by returned rows; otherwise (an unredaction deleted rows, or a
//! re-redaction re-stamped them) it re-lists from epoch 0 and replaces the set
//! (`docs/contracts/storage-semantics.md` 4.5 consumer rule).

use parking_lot::{Mutex, RwLock};
use scion_core::{ObjectId, Result};
use scion_obs::Metrics;
use scion_store::MetaStore;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The smallest refresh period, so a zero bound cannot spin.
const MIN_BOUND: Duration = Duration::from_millis(10);

/// Redacted ids known to this server and the epoch they reflect (SPEC 13.3).
pub struct RedactionSet {
    meta: Arc<dyn MetaStore>,
    ids: RwLock<HashSet<ObjectId>>,
    epoch: AtomicU64,
    last_refresh: Mutex<Instant>,
    /// Serializes refreshes so the epoch never moves backwards.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Wakes the refresher when a newer epoch was observed.
    notify: Arc<Notify>,
    /// Whether [`RedactionSet::spawn_refresher`] runs a refresher task.
    refresher: AtomicBool,
    /// `scion_redaction_epoch{tier=server}` / `scion_redaction_lag_seconds{tier=server}`.
    metrics: RwLock<Option<Arc<Metrics>>>,
}

impl RedactionSet {
    /// An empty set over `meta` (no IO).
    pub fn new(meta: Arc<dyn MetaStore>) -> Arc<Self> {
        Arc::new(RedactionSet {
            meta,
            ids: RwLock::new(HashSet::new()),
            epoch: AtomicU64::new(0),
            last_refresh: Mutex::new(Instant::now()),
            refresh_lock: tokio::sync::Mutex::new(()),
            notify: Arc::new(Notify::new()),
            refresher: AtomicBool::new(false),
            metrics: RwLock::new(None),
        })
    }

    /// Report refreshes to `scion_redaction_epoch{tier=server}` and
    /// `scion_redaction_lag_seconds{tier=server}` (SPEC 15.2).
    pub fn set_metrics(&self, m: Arc<Metrics>) {
        m.redaction_epoch
            .with_label_values(&["server"])
            .set(i64::try_from(self.epoch()).unwrap_or(i64::MAX));
        *self.metrics.write() = Some(m);
    }

    /// True when `id` is redacted (quarantined or purged; SPEC 13.4).
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.ids.read().contains(id)
    }

    /// The redaction epoch this set reflects (SPEC 13.3).
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Number of redacted ids in the set.
    pub fn len(&self) -> usize {
        self.ids.read().len()
    }

    /// Whether the set holds no ids.
    pub fn is_empty(&self) -> bool {
        self.ids.read().is_empty()
    }

    /// Reload from the metadata store; returns the epoch (SPEC 13.3).
    pub async fn refresh(&self) -> Result<u64> {
        let _serial = self.refresh_lock.lock().await;
        let since = self.epoch();
        let (rows, current) = self.meta.list_redactions(since).await?;
        if current == since && rows.is_empty() {
            self.refreshed(current);
            return Ok(current);
        }
        // Incremental only when every increment in (since, current] has rows carrying it.
        let incremental = current > since && {
            let gap = current - since;
            let covered: BTreeSet<u64> = rows.iter().map(|r| r.epoch).collect();
            gap <= covered.len() as u64 && (since + 1..=current).all(|e| covered.contains(&e))
        };
        if incremental {
            let mut ids = self.ids.write();
            ids.extend(rows.iter().map(|r| r.object));
        } else {
            let (all, current_all) = self.meta.list_redactions(0).await?;
            let fresh: HashSet<ObjectId> = all.iter().map(|r| r.object).collect();
            *self.ids.write() = fresh;
            self.epoch.store(current_all, Ordering::Release);
            self.refreshed(current_all);
            return Ok(current_all);
        }
        self.epoch.store(current, Ordering::Release);
        self.refreshed(current);
        Ok(current)
    }

    fn refreshed(&self, epoch: u64) {
        *self.last_refresh.lock() = Instant::now();
        if let Some(m) = self.metrics.read().as_ref() {
            m.redaction_epoch
                .with_label_values(&["server"])
                .set(i64::try_from(epoch).unwrap_or(i64::MAX));
            m.redaction_lag.with_label_values(&["server"]).set(0.0);
        }
    }

    fn report_lag(&self) {
        if let Some(m) = self.metrics.read().as_ref() {
            m.redaction_lag
                .with_label_values(&["server"])
                .set(self.last_refresh().elapsed().as_secs_f64());
        }
    }

    /// Note an epoch observed elsewhere; triggers a refresh when it is newer than the set's
    /// (SPEC 13.3: refresh whenever a higher epoch is observed).
    pub fn observe(self: &Arc<Self>, epoch: u64) {
        if epoch <= self.epoch() {
            return;
        }
        if self.refresher.load(Ordering::Acquire) {
            self.notify.notify_one();
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let me = self.clone();
            rt.spawn(async move {
                if let Err(e) = me.refresh().await {
                    tracing::warn!(error = %e, "redaction set refresh failed");
                }
            });
        }
    }

    /// When the last successful refresh happened.
    pub fn last_refresh(&self) -> Instant {
        *self.last_refresh.lock()
    }

    /// Refresh at least every `bound`, and immediately when [`RedactionSet::observe`] sees a
    /// newer epoch (SPEC 13.3). The task stops when the set is dropped or the handle aborted.
    pub fn spawn_refresher(self: &Arc<Self>, bound: Duration) -> tokio::task::JoinHandle<()> {
        let bound = bound.max(MIN_BOUND);
        self.refresher.store(true, Ordering::Release);
        let weak = Arc::downgrade(self);
        let notify = self.notify.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(bound) => {}
                    _ = notify.notified() => {}
                }
                let Some(me) = weak.upgrade() else {
                    break;
                };
                if let Err(e) = me.refresh().await {
                    tracing::warn!(error = %e, "redaction set refresh failed");
                }
                me.report_lag();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Principal;
    use scion_store::{AuditRow, MemMetaStore, MemOptions, ObjectRow, ObjectState, RedactTxn};
    use std::time::SystemTime;

    async fn object(meta: &MemMetaStore, data: &[u8]) -> ObjectId {
        let id = ObjectId::hash_blob(data);
        meta.insert_object(ObjectRow {
            id,
            kind: scion_core::Kind::Blob,
            size: data.len() as u64,
            stored_size: data.len() as u64,
            inline_data: None,
            state: ObjectState::Live,
            created_at: SystemTime::UNIX_EPOCH,
        })
        .await
        .unwrap();
        id
    }

    fn txn(ids: Vec<ObjectId>) -> RedactTxn {
        RedactTxn {
            ids,
            reason: "r".into(),
            actor: Principal::Service("t".into()).id_string(),
            at: SystemTime::UNIX_EPOCH,
            purge_after: SystemTime::UNIX_EPOCH,
        }
    }

    fn audit() -> AuditRow {
        AuditRow {
            at: SystemTime::UNIX_EPOCH,
            actor: "service:t".into(),
            action: "unredact".into(),
            target: String::new(),
            reason: String::new(),
            detail: String::new(),
        }
    }

    #[tokio::test]
    async fn refresh_tracks_redact_and_unredact() {
        let meta = Arc::new(MemMetaStore::new(MemOptions::default()));
        let a = object(&meta, b"a").await;
        let b = object(&meta, b"b").await;
        let set = RedactionSet::new(meta.clone());
        let metrics = Metrics::new();
        set.set_metrics(metrics.clone());
        assert_eq!(set.refresh().await.unwrap(), 0);
        assert!(!set.contains(&a));

        meta.redact(txn(vec![a])).await.unwrap();
        meta.redact(txn(vec![b])).await.unwrap();
        assert_eq!(set.refresh().await.unwrap(), 2);
        assert!(set.contains(&a) && set.contains(&b));
        assert_eq!(set.len(), 2);
        assert_eq!(
            metrics.redaction_epoch.with_label_values(&["server"]).get(),
            2
        );

        meta.unredact(&[a], audit()).await.unwrap();
        assert_eq!(set.refresh().await.unwrap(), 3);
        assert!(!set.contains(&a) && set.contains(&b));

        // Re-redacting re-stamps the row: epochs 4 and 5 both touch `b`, so epoch 4 has no
        // row and the set must re-list rather than miss anything.
        meta.redact(txn(vec![a])).await.unwrap();
        meta.redact(txn(vec![a, b])).await.unwrap();
        assert_eq!(set.refresh().await.unwrap(), 5);
        assert!(set.contains(&a) && set.contains(&b));
    }

    #[tokio::test]
    async fn refresher_and_observe() {
        let meta = Arc::new(MemMetaStore::new(MemOptions::default()));
        let a = object(&meta, b"a").await;
        let set = RedactionSet::new(meta.clone());
        let before = set.last_refresh();
        let task = set.spawn_refresher(Duration::from_secs(3600));
        let epoch = meta.redact(txn(vec![a])).await.unwrap();
        set.observe(epoch);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !set.contains(&a) {
            assert!(
                Instant::now() < deadline,
                "observe did not trigger a refresh"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(set.epoch(), epoch);
        assert!(set.last_refresh() > before);
        task.abort();

        // Without a refresher, observe spawns a one-off refresh.
        let set2 = RedactionSet::new(meta.clone());
        set2.observe(epoch);
        while !set2.contains(&a) {
            assert!(Instant::now() < deadline, "one-off refresh did not run");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // An old epoch is ignored.
        set2.observe(0);
    }

    #[tokio::test]
    async fn periodic_refresh_within_bound() {
        let meta = Arc::new(MemMetaStore::new(MemOptions::default()));
        let a = object(&meta, b"a").await;
        let set = RedactionSet::new(meta.clone());
        let task = set.spawn_refresher(Duration::from_millis(20));
        meta.redact(txn(vec![a])).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !set.contains(&a) {
            assert!(Instant::now() < deadline, "periodic refresh did not run");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
    }
}
