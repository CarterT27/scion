//! Working-copy library (SPEC 3.2 level 7, SPEC 11; PLAN 2.17).
//!
//! Local layer: [`fs_safe`], [`index`] (stat cache), [`status`], [`ignore`], [`state`],
//! [`lock`], [`credentials`]. Remote layer: [`WorkingCopy`] checkout, goto, sparse, fetch,
//! placeholders, redaction refresh and recovery. Write layer: commit, amend, rebase with
//! conflict materialization, resolve and land. The `testing` feature adds reusable scenarios.
#![warn(missing_docs)]

pub mod credentials;
pub mod fs_safe;
pub mod ignore;
pub mod index;
pub mod lock;
pub mod points;
pub mod state;
pub mod status;

mod checkout;
mod commit;
mod diff;
mod fetch;
mod goto;
mod land;
mod placeholder;
mod rebase;
mod recover;
mod redaction_refresh;
mod remote;
mod resolve;
mod serde_util;
mod sparse;
mod tree_cache;
mod wc;

#[cfg(feature = "testing")]
pub mod testing;
#[cfg(feature = "testing")]
mod testing_write;

pub use diff::FileChangeView;
pub use goto::GotoReport;
pub use land::LandReport;
pub use placeholder::{REDACTED_PLACEHOLDER_PREFIX, placeholder_bytes};
pub use rebase::RebaseReport;
pub use wc::{WcOptions, WorkingCopy};
