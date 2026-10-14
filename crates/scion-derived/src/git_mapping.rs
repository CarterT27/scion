//! SPEC 9.2 `git_mapping` (eager, import mode only; PLAN 6.4; P21).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::engine::DerivedEngine;
use crate::types::{DeriveCtx, DeriveMode, DerivedType, EagerType};
use async_trait::async_trait;
use scion_core::{ObjectId, Result};
use scion_history::EagerInput;
use scion_store::DerivedRow;

/// Type name.
pub const NAME: &str = "git_mapping";
/// Type version.
pub const VERSION: u32 = 1;

/// Emits `g:<40hex> -> changeset` and `c:<changeset id bytes> -> 40hex` when the changeset is
/// created in import mode and its Meta carries extension tag 2.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitMappingType;

impl DerivedType for GitMappingType {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn mode(&self) -> DeriveMode {
        DeriveMode::Eager
    }

    fn content_bearing(&self) -> bool {
        false
    }
}

#[async_trait]
impl EagerType for GitMappingType {
    async fn derive(&self, cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        todo!("P21")
    }
}

/// The changeset imported from git commit `sha_hex` (40 hex).
pub async fn changeset_for_git(e: &DerivedEngine, sha_hex: &str) -> Result<Option<ObjectId>> {
    todo!("P21")
}

/// The git commit (40 hex) a changeset was imported from.
pub async fn git_for_changeset(e: &DerivedEngine, cs: &ObjectId) -> Result<Option<String>> {
    todo!("P21")
}
