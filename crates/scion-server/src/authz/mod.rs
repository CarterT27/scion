//! Object read authorization and policy adapters (SPEC 12.4, 12.5, 18.6; PLAN 2.14).
//!
//! `can_read` lives in `object.rs` (P25); the policy adapters in `policy_adapters.rs` (P26).
#![allow(dead_code)] // P00: fields are read by object.rs (P25).

mod object;
mod policy_adapters;

pub use policy_adapters::{PolicyLandAuthzSource, PolicyWriteAuthz};

use crate::redaction_set::RedactionSet;
use scion_policy::PolicyHandle;
use scion_store::Storage;
use std::sync::Arc;

/// Access class of a readable object (SPEC 7.3, 10.4).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AccessClass {
    /// Governed by the `/` read rule.
    Root,
    /// Governed by a more specific rule.
    Restricted,
}

/// SPEC 18.6 decision.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadDecision {
    /// Readable with this access class.
    Allow(AccessClass),
    /// Not readable (or unknown).
    Deny,
    /// Redacted.
    Redacted,
}

/// Retroactive object authorization via `object_paths` (SPEC 12.5, 18.6), with a
/// `(principal, id, policy_epoch)` decision cache.
pub struct ObjectAuthorizer {
    storage: Storage,
    policy: Arc<PolicyHandle>,
    redactions: Arc<RedactionSet>,
    /// Decision cache and other private state; its type lives in `object.rs` (P25).
    state: object::AuthzState,
}

impl ObjectAuthorizer {
    /// Assemble the authorizer (no IO).
    pub fn new(storage: Storage, policy: Arc<PolicyHandle>, redactions: Arc<RedactionSet>) -> Self {
        ObjectAuthorizer {
            storage,
            policy,
            redactions,
            state: object::AuthzState::default(),
        }
    }
}
