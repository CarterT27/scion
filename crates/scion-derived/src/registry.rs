//! Derived type registry (SPEC 9.1; P14).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::types::{DeriveMode, EagerType, LazyType};
use std::sync::Arc;

/// Registered derived types, looked up by name and version.
#[derive(Clone, Default)]
pub struct Registry {
    eager: Vec<Arc<dyn EagerType>>,
    lazy: Vec<Arc<dyn LazyType>>,
    descriptors: Vec<(&'static str, u32, DeriveMode, bool)>,
}

impl Registry {
    /// A registry with no types.
    pub fn empty() -> Self {
        Registry::default()
    }

    /// The v1 registry: descriptors `commit_graph` and `path_index`; eager `history` and
    /// `git_mapping`; lazy `blame` (SPEC 9.2).
    pub fn v1_default() -> Self {
        todo!("P14")
    }

    /// Register an eager type.
    pub fn register_eager(&mut self, t: Arc<dyn EagerType>) {
        todo!("P14")
    }

    /// Register a lazy type.
    pub fn register_lazy(&mut self, t: Arc<dyn LazyType>) {
        todo!("P14")
    }

    /// Register a descriptor-only type computed elsewhere (commit_graph, path_index).
    pub fn register_descriptor(
        &mut self,
        name: &'static str,
        version: u32,
        mode: DeriveMode,
        content_bearing: bool,
    ) {
        todo!("P14")
    }

    /// The latest registered version of `name`.
    pub fn latest_version(&self, name: &str) -> Option<u32> {
        todo!("P14")
    }

    /// Names of content-bearing types (SPEC 13.5).
    pub fn content_bearing_types(&self) -> Vec<String> {
        todo!("P14")
    }
}
