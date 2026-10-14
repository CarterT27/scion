//! Single-flight request coalescing (SPEC 10.2; P10).

use parking_lot::Mutex;
use scion_core::{Result, ScionError};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

/// One in-flight fetch result slot.
type Slot<V> = Arc<tokio::sync::OnceCell<std::result::Result<V, ScionError>>>;

/// Concurrent callers for the same key share one fetch; errors are shared too (SPEC 10.2
/// request coalescing: concurrent requests for the same missing object produce exactly one
/// upstream request).
///
/// The key's entry is removed once the fetch completes, so a later call fetches again. If the
/// caller running the fetch is cancelled, another waiting caller takes over with its own
/// fetch function.
pub struct Coalescer<K, V> {
    inflight: Mutex<HashMap<K, Slot<V>>>,
}

/// Removes the key's slot when the group is done: either the result is in, or the last
/// participant left without one (cancellation).
struct SlotGuard<'a, K: Eq + Hash, V> {
    map: &'a Mutex<HashMap<K, Slot<V>>>,
    key: K,
    slot: Option<Slot<V>>,
}

impl<K: Eq + Hash, V> Drop for SlotGuard<'_, K, V> {
    fn drop(&mut self) {
        let mut map = self.map.lock();
        // Release our reference under the lock: every clone and drop of a slot happens under
        // this lock, so the strong count below is exact.
        let Some(mine) = self.slot.take() else {
            return;
        };
        let Some(current) = map.get(&self.key) else {
            return;
        };
        if !Arc::ptr_eq(current, &mine) {
            return;
        }
        drop(mine);
        if current.initialized() || Arc::strong_count(current) == 1 {
            map.remove(&self.key);
        }
    }
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

    /// Run `f` once per concurrent group of callers for `k`; every caller of the group gets a
    /// clone of the same result, error included (SPEC 10.2).
    pub async fn get_or_fetch<F, Fut>(&self, k: K, f: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>> + Send,
    {
        let slot = {
            let mut map = self.inflight.lock();
            Arc::clone(map.entry(k.clone()).or_default())
        };
        let guard = SlotGuard {
            map: &self.inflight,
            key: k,
            slot: Some(slot),
        };
        let result = match guard.slot.as_ref() {
            Some(slot) => slot.get_or_init(f).await.clone(),
            None => f().await,
        };
        drop(guard);
        result
    }

    /// Number of keys with a fetch in flight (diagnostics and tests).
    pub fn inflight(&self) -> usize {
        self.inflight.lock().len()
    }
}

impl<K: Eq + Hash + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static> Default
    for Coalescer<K, V>
{
    fn default() -> Self {
        Coalescer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::ErrorCode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_leader_is_replaced() {
        let c: Arc<Coalescer<u32, u32>> = Arc::new(Coalescer::new());
        let c2 = Arc::clone(&c);
        let leader = tokio::spawn(async move {
            c2.get_or_fetch(1, || async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(0)
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(c.inflight(), 1);
        leader.abort();
        let _ = leader.await;
        // The abandoned slot was cleaned up.
        assert_eq!(c.inflight(), 0);
        assert_eq!(c.get_or_fetch(1, || async { Ok(7) }).await.unwrap(), 7);
        assert_eq!(c.inflight(), 0);
    }

    #[tokio::test]
    async fn waiter_takes_over_when_leader_cancelled() {
        let c: Arc<Coalescer<u32, u32>> = Arc::new(Coalescer::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let (c1, r1) = (Arc::clone(&c), Arc::clone(&runs));
        let leader = tokio::spawn(async move {
            c1.get_or_fetch(1, move || async move {
                r1.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(0)
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (c2, r2) = (Arc::clone(&c), Arc::clone(&runs));
        let waiter = tokio::spawn(async move {
            c2.get_or_fetch(1, move || async move {
                r2.fetch_add(1, Ordering::SeqCst);
                Ok(5)
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        leader.abort();
        assert_eq!(waiter.await.unwrap().unwrap(), 5);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(c.inflight(), 0);
    }

    #[tokio::test]
    async fn sequential_calls_fetch_again() {
        let c: Coalescer<&'static str, u32> = Coalescer::new();
        let runs = AtomicUsize::new(0);
        for i in 0..3 {
            let v = c
                .get_or_fetch("k", || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok(i)
                })
                .await
                .unwrap();
            assert_eq!(v, i);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let e = c
            .get_or_fetch("k", || async {
                Err(ScionError::unavailable("upstream_unavailable", "down"))
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert_eq!(c.inflight(), 0);
    }
}
