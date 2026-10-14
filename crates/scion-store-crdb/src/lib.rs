//! CockroachDB [`scion_store::MetaStore`] backend (SPEC 5.2-5.4; PLAN 2.4): SERIALIZABLE
//! transactions with a bounded 40001 retry loop, follower reads for `Consistency::Stale`, and
//! embedded migrations following `docs/contracts/storage-semantics.md`.
#![warn(missing_docs)]

mod config;
mod migrations;
mod store;

pub use config::{CrdbConfig, test_url};
pub use migrations::MIGRATIONS;
pub use store::CrdbMetaStore;
