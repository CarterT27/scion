//! Derived type registry (SPEC 9.1; P14).
//!
//! Types are registered by `(name, version)`. Several versions of one name may be registered at
//! once (SPEC 9.1: "bump to re-derive; old versions remain readable"); queries without an
//! explicit version select [`Registry::latest_version`]. Registering the same `(name, version)`
//! twice replaces the earlier registration.

use crate::types::{DeriveMode, DerivedType, EagerType, LazyType};
use crate::{blame, commit_graph, git_mapping, history, path_index};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Registered derived types, looked up by name and version.
#[derive(Clone, Default)]
pub struct Registry {
    eager: Vec<Arc<dyn EagerType>>,
    lazy: Vec<Arc<dyn LazyType>>,
    descriptors: Vec<(&'static str, u32, DeriveMode, bool)>,
}

/// What the registry knows about one registered `(name, version)` (SPEC 9.1 `DerivedType`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TypeInfo {
    /// Stable type name.
    pub name: &'static str,
    /// Type version.
    pub version: u32,
    /// Eager or lazy.
    pub mode: DeriveMode,
    /// Stores file content (SPEC 13.5).
    pub content_bearing: bool,
    /// True for descriptor-only types computed outside the engine (`commit_graph`,
    /// `path_index`; SPEC 9.2).
    pub descriptor: bool,
}

fn info_of(t: &dyn DerivedType, descriptor: bool) -> TypeInfo {
    TypeInfo {
        name: t.name(),
        version: t.version(),
        mode: t.mode(),
        content_bearing: t.content_bearing(),
        descriptor,
    }
}

impl Registry {
    /// A registry with no types.
    pub fn empty() -> Self {
        Registry::default()
    }

    /// The v1 registry: descriptors `commit_graph` and `path_index`; eager `history` and
    /// `git_mapping`; lazy `blame` (SPEC 9.2).
    pub fn v1_default() -> Self {
        let mut r = Registry::empty();
        r.register_descriptor(
            commit_graph::NAME,
            commit_graph::VERSION,
            DeriveMode::Eager,
            false,
        );
        r.register_descriptor(
            path_index::NAME,
            path_index::VERSION,
            DeriveMode::Eager,
            false,
        );
        r.register_eager(Arc::new(history::HistoryType));
        r.register_eager(Arc::new(git_mapping::GitMappingType));
        r.register_lazy(Arc::new(blame::BlameType::new()));
        r
    }

    /// Drop any earlier registration of `(name, version)`: one definition per name and
    /// version (SPEC 9.1).
    fn remove(&mut self, name: &str, version: u32) {
        self.eager
            .retain(|t| !(t.name() == name && t.version() == version));
        self.lazy
            .retain(|t| !(t.name() == name && t.version() == version));
        self.descriptors
            .retain(|d| !(d.0 == name && d.1 == version));
    }

    /// Register an eager type (SPEC 9.1). Replaces an earlier registration of the same name
    /// and version.
    pub fn register_eager(&mut self, t: Arc<dyn EagerType>) {
        self.remove(t.name(), t.version());
        self.eager.push(t);
    }

    /// Register a lazy type (SPEC 9.1). Replaces an earlier registration of the same name and
    /// version.
    pub fn register_lazy(&mut self, t: Arc<dyn LazyType>) {
        self.remove(t.name(), t.version());
        self.lazy.push(t);
    }

    /// Register a descriptor-only type computed elsewhere (commit_graph, path_index; SPEC 9.2).
    pub fn register_descriptor(
        &mut self,
        name: &'static str,
        version: u32,
        mode: DeriveMode,
        content_bearing: bool,
    ) {
        self.remove(name, version);
        self.descriptors
            .push((name, version, mode, content_bearing));
    }

    /// Every registration: eager types, then lazy types, then descriptors, each in
    /// registration order (SPEC 9.1).
    pub fn types(&self) -> Vec<TypeInfo> {
        let mut out: Vec<TypeInfo> = Vec::new();
        out.extend(self.eager.iter().map(|t| info_of(t.as_ref(), false)));
        out.extend(self.lazy.iter().map(|t| info_of(t.as_ref(), false)));
        out.extend(
            self.descriptors
                .iter()
                .map(|&(name, version, mode, content_bearing)| TypeInfo {
                    name,
                    version,
                    mode,
                    content_bearing,
                    descriptor: true,
                }),
        );
        out
    }

    /// The registration of `(name, version)`, if any (SPEC 9.1).
    pub fn info(&self, name: &str, version: u32) -> Option<TypeInfo> {
        self.types()
            .into_iter()
            .find(|t| t.name == name && t.version == version)
    }

    /// True when any version of `name` is registered (SPEC 9.1).
    pub fn contains(&self, name: &str) -> bool {
        self.latest_version(name).is_some()
    }

    /// The registered versions of `name`, ascending (SPEC 9.1: old versions remain readable).
    pub fn versions(&self, name: &str) -> Vec<u32> {
        let set: BTreeSet<u32> = self
            .types()
            .into_iter()
            .filter(|t| t.name == name)
            .map(|t| t.version)
            .collect();
        set.into_iter().collect()
    }

    /// The latest registered version of `name` (SPEC 9.1 version selection).
    pub fn latest_version(&self, name: &str) -> Option<u32> {
        self.types()
            .into_iter()
            .filter(|t| t.name == name)
            .map(|t| t.version)
            .max()
    }

    /// Names of content-bearing types, sorted and deduplicated (SPEC 13.5).
    pub fn content_bearing_types(&self) -> Vec<String> {
        let set: BTreeSet<&'static str> = self
            .types()
            .into_iter()
            .filter(|t| t.content_bearing)
            .map(|t| t.name)
            .collect();
        set.into_iter().map(str::to_owned).collect()
    }

    /// The registered eager types (every version), in registration order (SPEC 9.1, 5.3).
    pub fn eager_types(&self) -> &[Arc<dyn EagerType>] {
        &self.eager
    }

    /// The registered lazy types (every version), in registration order (SPEC 9.1).
    pub fn lazy_types(&self) -> &[Arc<dyn LazyType>] {
        &self.lazy
    }

    /// The eager type registered as `(name, version)` (SPEC 9.1).
    pub fn eager(&self, name: &str, version: u32) -> Option<Arc<dyn EagerType>> {
        self.eager
            .iter()
            .find(|t| t.name() == name && t.version() == version)
            .cloned()
    }

    /// The lazy type registered as `(name, version)` (SPEC 9.1).
    pub fn lazy(&self, name: &str, version: u32) -> Option<Arc<dyn LazyType>> {
        self.lazy
            .iter()
            .find(|t| t.name() == name && t.version() == version)
            .cloned()
    }

    /// Every eager `(name, version)` pair, types and descriptors alike (the input to
    /// `FsckOptions::required_eager`; SPEC 4.5, 5.3).
    pub fn eager_pairs(&self) -> Vec<(String, u32)> {
        self.types()
            .into_iter()
            .filter(|t| t.mode == DeriveMode::Eager)
            .map(|t| (t.name.to_owned(), t.version))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_default_contents() {
        let r = Registry::v1_default();
        assert_eq!(r.latest_version("commit_graph"), Some(1));
        assert_eq!(r.latest_version("path_index"), Some(1));
        assert_eq!(r.latest_version("history"), Some(1));
        assert_eq!(r.latest_version("git_mapping"), Some(1));
        assert_eq!(r.latest_version("blame"), Some(1));
        assert_eq!(r.latest_version("nope"), None);
        assert!(r.content_bearing_types().is_empty());
        assert!(r.lazy("blame", 1).is_some());
        assert!(r.eager("history", 1).is_some());
        assert!(r.eager("git_mapping", 1).is_some());
        assert_eq!(r.info("blame", 1).map(|i| i.mode), Some(DeriveMode::Lazy));
        assert_eq!(r.info("commit_graph", 1).map(|i| i.descriptor), Some(true));
        let pairs = r.eager_pairs();
        for name in ["history", "git_mapping", "commit_graph", "path_index"] {
            assert!(pairs.contains(&(name.to_owned(), 1)), "{name}");
        }
        assert!(!pairs.iter().any(|(n, _)| n == "blame"));
    }

    #[test]
    fn versions_and_replacement() {
        let mut r = Registry::empty();
        r.register_lazy(Arc::new(blame::BlameType::new()));
        r.register_lazy(Arc::new(blame::BlameType::with_version(3)));
        r.register_lazy(Arc::new(blame::BlameType::with_version(2)));
        assert_eq!(r.latest_version("blame"), Some(3));
        assert_eq!(r.versions("blame"), vec![1, 2, 3]);
        r.register_lazy(Arc::new(blame::BlameType::with_version(2)));
        assert_eq!(r.lazy_types().len(), 3);
        r.register_descriptor("blame", 2, DeriveMode::Lazy, true);
        assert_eq!(r.lazy_types().len(), 2);
        assert_eq!(r.content_bearing_types(), vec!["blame".to_owned()]);
        assert!(r.contains("blame"));
    }
}
