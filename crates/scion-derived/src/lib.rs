//! Derived data framework and the v1 types (SPEC 3.2 level 4, SPEC 9; PLAN 2.7).
//!
//! - [`types`]: the frozen framework traits ([`DerivedType`], [`DeriveCtx`], [`EagerType`],
//!   [`LazyType`]).
//! - [`Registry`], [`DerivedEngine`] (implements `scion_history::EagerHook`) and
//!   [`StoreDeriveCtx`].
//! - v1 types: [`history`] (eager, kind 0x10 nodes), [`path_index`] and [`commit_graph`]
//!   (descriptors computed by scion-history), [`git_mapping`] (eager), [`blame`] (lazy), plus
//!   [`log`] (Log(path)) and [`versions`] (all versions of a path).
#![warn(missing_docs)]

pub mod types;

mod ctx;
mod engine;
mod lease;
mod registry;

#[path = "history_type.rs"]
pub mod history;

pub mod blame;
pub mod git_mapping;
pub mod log;
pub mod path_index;
pub mod versions;

/// SPEC 9.2 `commit_graph` (eager): stored on the changesets row by scion-history.
pub mod commit_graph {
    /// Type name.
    pub const NAME: &str = "commit_graph";
    /// Type version.
    pub const VERSION: u32 = 1;
}

pub use ctx::StoreDeriveCtx;
pub use engine::DerivedEngine;
pub use registry::Registry;
pub use types::{
    DeriveConfig, DeriveCtx, DeriveMetrics, DeriveMode, DerivedType, EagerType, LazyType,
};
