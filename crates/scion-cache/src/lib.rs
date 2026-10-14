//! Cache layer primitives (SPEC 3.2 level 6, SPEC 10; PLAN 2.15): the verified
//! content-addressed disk cache, single-flight coalescing, the upstream limiter, the ref TTL
//! cache, local redaction tracking and the per-principal access cache.
#![warn(missing_docs)]

mod access;
mod coalesce;
mod disk;
mod limiter;
mod redactions;
mod refcache;

pub use access::AccessCache;
pub use coalesce::Coalescer;
pub use disk::{DiskCache, Payload};
pub use limiter::{LimiterPermit, UpstreamLimiter};
pub use redactions::LocalRedactions;
pub use refcache::RefCache;
