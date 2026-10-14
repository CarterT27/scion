//! SPEC 9.2 `history` (eager): history nodes of derived kind 0x10 (P21).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::types::{DeriveCtx, DeriveMode, DerivedType, EagerType};
use async_trait::async_trait;
use scion_core::{ObjectId, Result};
use scion_history::EagerInput;
use scion_object::{HistoryNode, RepoPath};
use scion_store::DerivedRow;

/// Type name.
pub const NAME: &str = "history";
/// Type version.
pub const VERSION: u32 = 1;

/// The eager history-node type (not content-bearing).
#[derive(Clone, Copy, Debug, Default)]
pub struct HistoryType;

impl DerivedType for HistoryType {
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
impl EagerType for HistoryType {
    async fn derive(&self, cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        todo!("P21")
    }
}

/// The root history node id of changeset `cs`.
pub async fn root_node(cx: &dyn DeriveCtx, cs: &ObjectId) -> Result<ObjectId> {
    todo!("P21")
}

/// The node for `path` in changeset `cs`, if the path exists.
pub async fn node_for_path(
    cx: &dyn DeriveCtx,
    cs: &ObjectId,
    path: &RepoPath,
) -> Result<Option<(ObjectId, HistoryNode)>> {
    todo!("P21")
}

/// Decode the node `id`.
pub async fn get_node(cx: &dyn DeriveCtx, id: &ObjectId) -> Result<HistoryNode> {
    todo!("P21")
}
