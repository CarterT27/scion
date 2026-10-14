//! The [`History`] engine (PLAN 2.6). The struct, constructor and accessors are here; the
//! behaviour lives in `impl History` blocks in the owning module files.

use crate::config::HistoryConfig;
use crate::hooks::{EagerHook, LandObserver, NoopLandObserver};
use scion_core::Clock;
use scion_store::{MetaStore, ObjectRepo};
use std::sync::Arc;

/// The history engine over an [`ObjectRepo`] (SPEC 8, 18).
#[allow(dead_code)] // P00: fields are read by the P12-P20 impl blocks.
pub struct History {
    pub(crate) objects: ObjectRepo,
    pub(crate) cfg: HistoryConfig,
    pub(crate) eager: Arc<dyn EagerHook>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) land_observer: Arc<dyn LandObserver>,
}

impl History {
    /// Assemble the engine (no IO). The land observer defaults to [`NoopLandObserver`].
    pub fn new(
        objects: ObjectRepo,
        cfg: HistoryConfig,
        eager: Arc<dyn EagerHook>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        History {
            objects,
            cfg,
            eager,
            clock,
            land_observer: Arc::new(NoopLandObserver),
        }
    }

    /// The object facade.
    pub fn objects(&self) -> &ObjectRepo {
        &self.objects
    }

    /// The metadata store.
    pub fn meta(&self) -> &Arc<dyn MetaStore> {
        &self.objects.storage().meta
    }

    /// The configuration.
    pub fn config(&self) -> &HistoryConfig {
        &self.cfg
    }

    /// Replace the land observer (metrics wiring, SPEC 15.2).
    pub fn with_land_observer(mut self, obs: Arc<dyn LandObserver>) -> Self {
        self.land_observer = obs;
        self
    }
}
