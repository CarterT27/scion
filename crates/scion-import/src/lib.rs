//! One-way git importer (SPEC 14; PLAN 2.19, 6.4): walks a git history with gix and recreates
//! it through the `scion.v1` RPCs in import mode. The `testing` feature adds fidelity helpers.
#![warn(missing_docs)]

mod import;
mod mapping;

#[cfg(feature = "testing")]
pub mod testing;

pub use import::{ImportOptions, ImportReport, import};
pub use mapping::{change_id_for, map_mode};
