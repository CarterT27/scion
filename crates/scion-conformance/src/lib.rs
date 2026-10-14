//! Cross-cutting SPEC 19 conformance harness (PLAN 2.22): [`TestCluster`] and the 16.2
//! invariant checks. The suites live in `tests/`.
#![warn(missing_docs)]

mod cluster;
pub mod invariants;
mod seed;
mod support;

pub use cluster::{Backend, ClusterOpts, TestCluster, Via};
