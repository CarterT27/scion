//! SPEC 8.4 / 18.4 lock-free Land (P20).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::hooks::{LandAuthorizerSource, Rebaser};
use crate::types::{LandRequest, LandResult};
use scion_core::{Principal, Result};

impl History {
    /// Land using `self` as the [`Rebaser`].
    pub async fn land(
        &self,
        req: LandRequest,
        who: &Principal,
        authz: &dyn LandAuthorizerSource,
    ) -> Result<LandResult> {
        todo!("P20")
    }

    /// Land with an explicit rebaser (tests use a fake).
    pub async fn land_with(
        &self,
        req: LandRequest,
        who: &Principal,
        authz: &dyn LandAuthorizerSource,
        rebaser: &dyn Rebaser,
    ) -> Result<LandResult> {
        todo!("P20")
    }
}
