//! The derivation engine (SPEC 9.1; P14).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::ctx::StoreDeriveCtx;
use crate::registry::Registry;
use crate::types::{DeriveConfig, DeriveCtx, DeriveMetrics};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use scion_core::{Clock, ObjectId, Result};
use scion_history::{EagerHook, EagerInput};
use scion_store::{DerivedRow, ObjectRepo};
use std::sync::Arc;

/// Runs eager types at changeset creation and lazy types on query (SPEC 9.1).
pub struct DerivedEngine {
    objects: ObjectRepo,
    registry: Registry,
    cfg: DeriveConfig,
    clock: Arc<dyn Clock>,
    ctx: StoreDeriveCtx,
    metrics: RwLock<Option<Arc<dyn DeriveMetrics>>>,
}

impl DerivedEngine {
    /// Assemble the engine (no IO).
    pub fn new(
        objects: ObjectRepo,
        registry: Registry,
        cfg: DeriveConfig,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let ctx = StoreDeriveCtx::new(objects.clone());
        Arc::new(DerivedEngine {
            objects,
            registry,
            cfg,
            clock,
            ctx,
            metrics: RwLock::new(None),
        })
    }

    /// The registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The derivation context.
    pub fn ctx(&self) -> &dyn DeriveCtx {
        &self.ctx
    }

    /// Install the metrics hook.
    pub fn set_metrics(&self, m: Arc<dyn DeriveMetrics>) {
        *self.metrics.write() = Some(m);
    }

    /// Generic query path (SPEC 9.1, 19.6): the stored value, or a lazy iterative derivation
    /// with write-back. `version = None` selects the latest registered version.
    pub async fn get(&self, ty: &str, version: Option<u32>, key: &[u8]) -> Result<Bytes> {
        todo!("P14")
    }

    /// The stored value only (no derivation).
    pub async fn get_stored(
        &self,
        ty: &str,
        version: Option<u32>,
        key: &[u8],
    ) -> Result<Option<Bytes>> {
        todo!("P14")
    }

    /// Delete content-bearing records keyed by source object `id` (SPEC 13.5).
    pub async fn purge_content_for(&self, id: &ObjectId) -> Result<u64> {
        todo!("P14")
    }
}

#[async_trait]
impl EagerHook for DerivedEngine {
    async fn derive_eager(&self, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        todo!("P14")
    }

    fn required_eager(&self) -> Vec<(String, u32)> {
        todo!("P14")
    }
}
