//! Bounded upstream concurrency (SPEC 10.2, 10.5; P10).

use scion_core::{ErrorCode, Result, ScionError};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

/// A semaphore of `max_concurrency` plus a bounded waiter count; overflow is
/// RESOURCE_EXHAUSTED immediately (SPEC 10.2: "excess requests queue, and queue overflow
/// returns RESOURCE_EXHAUSTED").
///
/// `max_concurrency = 0` means unlimited: `acquire` always succeeds at once.
pub struct UpstreamLimiter {
    sem: Arc<Semaphore>,
    max_queue: usize,
    waiting: AtomicUsize,
    unlimited: bool,
}

/// Decrements the waiter count when a queued `acquire` finishes or is cancelled.
struct WaitGuard<'a>(&'a AtomicUsize);

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl UpstreamLimiter {
    /// A limiter with the given bounds (SPEC 10.2 `upstream_max_concurrency`, PLAN 6.11
    /// `upstream_queue`).
    pub fn new(max_concurrency: usize, max_queue: usize) -> Self {
        UpstreamLimiter {
            sem: Arc::new(Semaphore::new(max_concurrency.min(Semaphore::MAX_PERMITS))),
            max_queue,
            waiting: AtomicUsize::new(0),
            unlimited: max_concurrency == 0,
        }
    }

    /// Wait for a slot. When all slots are busy and `max_queue` callers are already waiting,
    /// fails RESOURCE_EXHAUSTED immediately (SPEC 10.2).
    pub async fn acquire(&self) -> Result<LimiterPermit> {
        if self.unlimited {
            return Ok(LimiterPermit { _permit: None });
        }
        if let Ok(p) = Arc::clone(&self.sem).try_acquire_owned() {
            return Ok(LimiterPermit { _permit: Some(p) });
        }
        let reserved = self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |w| {
                (w < self.max_queue).then_some(w + 1)
            });
        if reserved.is_err() {
            tracing::debug!(
                max_queue = self.max_queue,
                "upstream limiter queue full; rejecting"
            );
            return Err(ScionError::resource_exhausted(format!(
                "upstream queue is full ({} waiting)",
                self.max_queue
            )));
        }
        let _guard = WaitGuard(&self.waiting);
        match Arc::clone(&self.sem).acquire_owned().await {
            Ok(p) => Ok(LimiterPermit { _permit: Some(p) }),
            Err(_) => Err(ScionError::new(
                ErrorCode::Unavailable,
                "upstream limiter is closed",
            )),
        }
    }

    /// Slots currently free (diagnostics).
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// Callers currently queued (diagnostics).
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }
}

/// A held upstream slot; released on drop.
pub struct LimiterPermit {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_blocks() {
        let l = UpstreamLimiter::new(0, 0);
        let mut v = Vec::new();
        for _ in 0..100 {
            v.push(l.acquire().await.unwrap());
        }
    }

    #[tokio::test]
    async fn cancelled_waiter_frees_queue_slot() {
        let l = Arc::new(UpstreamLimiter::new(1, 1));
        let p = l.acquire().await.unwrap();
        let l2 = Arc::clone(&l);
        let w = tokio::spawn(async move { l2.acquire().await.map(|_| ()) });
        while l.waiting() == 0 {
            tokio::task::yield_now().await;
        }
        w.abort();
        let _ = w.await;
        assert_eq!(l.waiting(), 0);
        drop(p);
        let _p = l.acquire().await.unwrap();
    }
}
