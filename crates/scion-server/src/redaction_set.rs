//! In-memory redaction set refreshed within the propagation bound (SPEC 13.3; P23).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::{Mutex, RwLock};
use scion_core::{ObjectId, Result};
use scion_store::MetaStore;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// Redacted ids known to this server and the epoch they reflect.
pub struct RedactionSet {
    meta: Arc<dyn MetaStore>,
    ids: RwLock<HashSet<ObjectId>>,
    epoch: AtomicU64,
    last_refresh: Mutex<Instant>,
}

impl RedactionSet {
    /// An empty set over `meta` (no IO).
    pub fn new(meta: Arc<dyn MetaStore>) -> Arc<Self> {
        Arc::new(RedactionSet {
            meta,
            ids: RwLock::new(HashSet::new()),
            epoch: AtomicU64::new(0),
            last_refresh: Mutex::new(Instant::now()),
        })
    }

    /// True when `id` is redacted.
    pub fn contains(&self, id: &ObjectId) -> bool {
        todo!("P23")
    }

    /// The redaction epoch this set reflects.
    pub fn epoch(&self) -> u64 {
        todo!("P23")
    }

    /// Reload from the metadata store; returns the epoch.
    pub async fn refresh(&self) -> Result<u64> {
        todo!("P23")
    }

    /// Note an epoch observed elsewhere (triggers a refresh when newer).
    pub fn observe(self: &Arc<Self>, epoch: u64) {
        todo!("P23")
    }

    /// When the last successful refresh happened.
    pub fn last_refresh(&self) -> Instant {
        todo!("P23")
    }

    /// Refresh at least every `bound`.
    pub fn spawn_refresher(self: &Arc<Self>, bound: Duration) -> tokio::task::JoinHandle<()> {
        todo!("P23")
    }
}
