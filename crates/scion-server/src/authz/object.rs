//! SPEC 18.6 `can_read_object` (P25).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use super::{ObjectAuthorizer, ReadDecision};
use scion_core::{ObjectId, Principal, Result};

/// Private per-authorizer state owned by P25: the `(principal, id, policy_epoch)` decision cache
/// of SPEC 18.6 / PLAN 2.14. P25 chooses the fields; `ObjectAuthorizer::new` builds it with
/// `Default`.
#[derive(Debug, Default)]
pub(crate) struct AuthzState {}

impl ObjectAuthorizer {
    /// Redacted first, then the decision cache, then `object_paths` rows of this id until the
    /// first readable path.
    pub async fn can_read(&self, p: &Principal, id: &ObjectId) -> Result<ReadDecision> {
        todo!("P25")
    }
}
