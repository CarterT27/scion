//! scion-history authorization traits over scion-policy (P26).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::Principal;
use scion_history::{LandAuthorizer, LandAuthorizerSource, WriteAuthorizer};
use scion_object::RepoPath;
use scion_policy::{Policy, PolicyHandle};
use std::sync::Arc;

/// [`WriteAuthorizer`] over one policy snapshot.
pub struct PolicyWriteAuthz {
    policy: Arc<Policy>,
    who: Principal,
}

impl PolicyWriteAuthz {
    /// Authorize `who` against `policy`.
    pub fn new(policy: Arc<Policy>, who: Principal) -> Self {
        PolicyWriteAuthz { policy, who }
    }
}

impl WriteAuthorizer for PolicyWriteAuthz {
    fn can_write(&self, path: &RepoPath) -> bool {
        todo!("P26")
    }

    fn can_import(&self) -> bool {
        todo!("P26")
    }
}

/// [`LandAuthorizerSource`] re-reading the policy handle on every land attempt.
pub struct PolicyLandAuthzSource {
    handle: Arc<PolicyHandle>,
    who: Principal,
}

impl PolicyLandAuthzSource {
    /// Authorize `who` against the live policy.
    pub fn new(handle: Arc<PolicyHandle>, who: Principal) -> Self {
        PolicyLandAuthzSource { handle, who }
    }
}

impl LandAuthorizerSource for PolicyLandAuthzSource {
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        todo!("P26")
    }
}
