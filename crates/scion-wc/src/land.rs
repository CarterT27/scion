//! SPEC 11.7 land from a working copy (P28).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::Result;
use scion_object::RefName;

/// Result of a land (`--json` schema `scion.cli.land.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LandReport {
    /// Ref name.
    pub ref_name: String,
    /// New ref target (id text).
    pub target: String,
    /// New ref version.
    pub version: u64,
    /// Landed results (id text).
    pub landed: Vec<String>,
}

impl WorkingCopy {
    /// Land the base onto `r`; on success the base becomes the landed result.
    pub async fn land(&mut self, r: &RefName) -> Result<LandReport> {
        todo!("P28")
    }
}
