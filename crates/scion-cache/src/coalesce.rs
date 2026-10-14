//! Single-flight request coalescing (SPEC 10.2; P10).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use parking_lot::Mutex;
use scion_core::{Result, ScionError};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

/// One in-flight fetch result slot.
type Slot<V> = Arc<tokio::sync::OnceCell<std::result::Result<V, ScionError>>>;

/// Concurrent callers for the same key share one fetch; errors are shared too.
pub struct Coalescer<K, V> {
    inflight: Mutex<HashMap<K, Slot<V>>>,
}

impl<K: Eq + Hash + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static>
    Coalescer<K, V>
{
    /// An empty coalescer.
    pub fn new() -> Self {
        Coalescer {
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Run `f` once per concurrent group of callers for `k`.
    pub async fn get_or_fetch<F, Fut>(&self, k: K, f: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>> + Send,
    {
        todo!("P10")
    }
}

impl<K: Eq + Hash + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static> Default
    for Coalescer<K, V>
{
    fn default() -> Self {
        Coalescer::new()
    }
}
