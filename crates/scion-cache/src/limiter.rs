//! Bounded upstream concurrency (SPEC 10.5; P10).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;

/// A semaphore of `max_concurrency` plus a bounded waiter count; overflow is
/// RESOURCE_EXHAUSTED immediately.
pub struct UpstreamLimiter {
    sem: Arc<Semaphore>,
    max_queue: usize,
    waiting: AtomicUsize,
}

impl UpstreamLimiter {
    /// A limiter with the given bounds.
    pub fn new(max_concurrency: usize, max_queue: usize) -> Self {
        UpstreamLimiter {
            sem: Arc::new(Semaphore::new(max_concurrency)),
            max_queue,
            waiting: AtomicUsize::new(0),
        }
    }

    /// Wait for a slot (RESOURCE_EXHAUSTED when the queue is full).
    pub async fn acquire(&self) -> Result<LimiterPermit> {
        todo!("P10")
    }
}

/// A held upstream slot; released on drop.
pub struct LimiterPermit {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
