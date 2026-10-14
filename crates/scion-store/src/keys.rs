//! SPEC 5.1 object storage key layout.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::types::{Area, RepoId};
use scion_core::ObjectId;

/// `<prefix>/{r|q}/<repo_id>/<kind_letter>/<hex[0:2]>/<hex>` (SPEC 5.1). Derived kinds use `x`
/// (kind 0x10) or `x<2-hex kind>` (`docs/contracts/storage-semantics.md` section 7).
pub fn object_key(prefix: &str, repo: RepoId, id: &ObjectId, area: Area) -> String {
    todo!("P03")
}
