//! `acl.toml` policy (SPEC 3.2 level 8, SPEC 6.2, 12.4; PLAN 2.9): parsing, validation,
//! evaluation, the policy epoch and hot reload. A leaf crate consumed by the service layer only.
#![warn(missing_docs)]

mod error;
mod handle;
mod policy;

pub use error::PolicyError;
pub use handle::PolicyHandle;
pub use policy::{AdminPerm, Policy};
