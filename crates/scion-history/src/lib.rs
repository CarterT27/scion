//! History layer (SPEC 3.2 level 3; PLAN 2.6): trees, apply_delta, FileDiff, CreateChangeset,
//! bootstrap, commit-graph queries, rebase/merge, refs, UpdateRef and lock-free Land.
//!
//! Upward needs are inverted through the traits in [`hooks`]: [`EagerHook`] (implemented by
//! scion-derived), [`WriteAuthorizer`], [`LandAuthorizer`], [`LandAuthorizerSource`] (implemented
//! by scion-server over scion-policy) and [`Rebaser`].
#![warn(missing_docs)]

pub mod config;
pub mod hooks;
pub mod types;

mod apply;
mod bootstrap;
mod conflict;
mod create;
mod diff;
mod graph;
mod graph_compute;
mod history;
mod land;
mod merge;
mod rebase;
mod refs;
mod tree;

pub use config::{HistoryConfig, LandConfig};
pub use graph_compute::compute_commit_graph;
pub use history::History;
pub use hooks::{
    AllowAll, EagerHook, EagerInput, LandAttemptOutcome, LandAuthorizer, LandAuthorizerSource,
    LandObserver, NoEager, NoopLandObserver, Rebaser, WriteAuthorizer,
};
pub use types::{
    Applied, ChangesetRecord, CreateRequest, Created, FileChange, LandRequest, LandResult,
    MetaFields, MetaSource, UpdateRefRequest,
};
