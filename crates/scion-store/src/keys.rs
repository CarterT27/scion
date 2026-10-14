//! SPEC 5.1 object storage key layout.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::types::{Area, RepoId};
use scion_core::ObjectId;

/// `<prefix>/{r|q}/<repo_id>/<kind_letter>/<hex[0:2]>/<hex>` (SPEC 5.1).
pub fn object_key(prefix: &str, repo: RepoId, id: &ObjectId, area: Area) -> String {
    todo!("P03")
}
