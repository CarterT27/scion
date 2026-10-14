//! The derivation engine (SPEC 9.1; P14).
//!
//! - Eager: [`EagerHook::derive_eager`] runs every registered eager type and concatenates their
//!   rows; History inserts them with the changeset (SPEC 5.3 step 2).
//! - Lazy: [`DerivedEngine::get`] returns the stored value or derives it. The derivation walks
//!   `deps()` with an explicit work stack (never recursion) down to the nearest stored values,
//!   then derives forward in dependency order, writing back every intermediate value with
//!   `put_derived` (`ON CONFLICT DO NOTHING`).
//! - Leases (`derive_leases`) only deduplicate concurrent work: a worker that cannot take the
//!   lease waits briefly for the value and then derives anyway (SPEC 9.1).

use crate::ctx::{StoreDeriveCtx, read_stored};
use crate::lease::{self, LeaseOutcome};
use crate::registry::Registry;
use crate::types::{DeriveConfig, DeriveCtx, DeriveMetrics, DeriveMode, LazyType};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use scion_core::{Clock, ErrorCode, ObjectId, Result, ScionError};
use scion_history::{EagerHook, EagerInput};
use scion_store::{DerivedKey, DerivedRow, ObjectRepo};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::debug;

/// Runs eager types at changeset creation and lazy types on query (SPEC 9.1).
pub struct DerivedEngine {
    objects: ObjectRepo,
    registry: Registry,
    cfg: DeriveConfig,
    clock: Arc<dyn Clock>,
    ctx: StoreDeriveCtx,
    metrics: RwLock<Option<Arc<dyn DeriveMetrics>>>,
    seq: AtomicU64,
}

/// One entry of the lazy work stack: a key and, once expanded, its dependency keys.
struct Frame {
    key: Bytes,
    deps: Option<Vec<Bytes>>,
}

/// Tracks the backlog gauge of one derivation; whatever is still outstanding when the
/// derivation ends (including on error) is subtracted on drop (SPEC 15.2).
struct Backlog {
    metrics: Option<Arc<dyn DeriveMetrics>>,
    ty: String,
    outstanding: i64,
}

impl Backlog {
    fn add(&mut self, n: i64) {
        self.outstanding += n;
        if let Some(m) = &self.metrics {
            m.backlog(&self.ty, n);
        }
    }
}

impl Drop for Backlog {
    fn drop(&mut self) {
        if self.outstanding != 0
            && let Some(m) = &self.metrics
        {
            m.backlog(&self.ty, -self.outstanding);
        }
    }
}

fn unknown_type(ty: &str) -> ScionError {
    ScionError::not_found(format!("unknown derived type {ty:?}"))
}

fn engine_bug(m: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, m)
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
            seq: AtomicU64::new(0),
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

    fn metrics(&self) -> Option<Arc<dyn DeriveMetrics>> {
        self.metrics.read().clone()
    }

    /// The version a query selects (SPEC 9.1): an explicit version is used as given (old
    /// versions stay readable even when no longer registered); `None` selects the latest
    /// registered version, or NOT_FOUND for an unknown type.
    pub fn resolve_version(&self, ty: &str, version: Option<u32>) -> Result<u32> {
        match version {
            Some(v) => Ok(v),
            None => self
                .registry
                .latest_version(ty)
                .ok_or_else(|| unknown_type(ty)),
        }
    }

    fn dkey(ty: &str, version: u32, key: &[u8]) -> DerivedKey {
        DerivedKey {
            ty: ty.to_owned(),
            version,
            key: Bytes::copy_from_slice(key),
        }
    }

    /// Generic query path (SPEC 9.1, 19.6): the stored value, or a lazy iterative derivation
    /// with write-back. `version = None` selects the latest registered version. NOT_FOUND for
    /// an unknown type, or for a non-lazy type (or unregistered version) with no stored value.
    pub async fn get(&self, ty: &str, version: Option<u32>, key: &[u8]) -> Result<Bytes> {
        let version = self.resolve_version(ty, version)?;
        if let Some(t) = self.registry.lazy(ty, version) {
            return self
                .derive_lazy(t, version, Bytes::copy_from_slice(key))
                .await;
        }
        match read_stored(&self.objects, &Self::dkey(ty, version, key)).await? {
            Some(v) => Ok(v),
            None if self.registry.contains(ty) => Err(ScionError::not_found(format!(
                "no {ty} v{version} value for the key"
            ))),
            None => Err(unknown_type(ty)),
        }
    }

    /// The stored value only, no derivation (SPEC 9.1). `version = None` selects the latest
    /// registered version (NOT_FOUND for an unknown type).
    pub async fn get_stored(
        &self,
        ty: &str,
        version: Option<u32>,
        key: &[u8],
    ) -> Result<Option<Bytes>> {
        let version = self.resolve_version(ty, version)?;
        read_stored(&self.objects, &Self::dkey(ty, version, key)).await
    }

    /// Delete content-bearing records keyed by source object `id` (SPEC 13.5). Records of
    /// every version of every registered content-bearing type are removed; non-content-bearing
    /// records are retained.
    pub async fn purge_content_for(&self, id: &ObjectId) -> Result<u64> {
        let types = self.registry.content_bearing_types();
        if types.is_empty() {
            return Ok(0);
        }
        let n = self
            .objects
            .storage()
            .meta
            .delete_derived_by_source(&types, id)
            .await?;
        debug!(deleted = n, "purged content-bearing derived records");
        Ok(n)
    }

    /// The iterative lazy derivation of `root` (SPEC 9.1): an explicit work stack expands
    /// `deps()` until stored values are reached, then derives forward, writing back every
    /// value. Memory is O(keys visited); the call stack does not grow with the chain length.
    async fn derive_lazy(&self, t: Arc<dyn LazyType>, version: u32, root: Bytes) -> Result<Bytes> {
        let ty = t.name();
        let holder = format!(
            "{}#{}",
            self.cfg.holder_id,
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        let mut backlog = Backlog {
            metrics: self.metrics(),
            ty: ty.to_owned(),
            outstanding: 0,
        };
        let mut values: HashMap<Bytes, Bytes> = HashMap::new();
        let mut expanding: HashSet<Bytes> = HashSet::new();
        let mut stack: Vec<Frame> = vec![Frame {
            key: root.clone(),
            deps: None,
        }];
        let mut contended = false;

        while let Some(top) = stack.last_mut() {
            if values.contains_key(&top.key) {
                stack.pop();
                continue;
            }
            let key = top.key.clone();
            match top.deps.take() {
                None => {
                    // First visit: a stored value ends the walk here.
                    if let Some(v) =
                        read_stored(&self.objects, &Self::dkey(ty, version, &key)).await?
                    {
                        values.insert(key, v);
                        stack.pop();
                        continue;
                    }
                    let deps = t.deps(&self.ctx, &key).await?;
                    let mut missing: Vec<Bytes> = Vec::new();
                    for d in &deps {
                        if values.contains_key(d) {
                            continue;
                        }
                        if expanding.contains(d) || *d == key {
                            return Err(engine_bug(format!(
                                "derived type {ty} v{version} has a dependency cycle"
                            )));
                        }
                        missing.push(d.clone());
                    }
                    expanding.insert(key);
                    backlog.add(1);
                    if let Some(f) = stack.last_mut() {
                        f.deps = Some(deps);
                    }
                    // Reverse so the first dependency is derived first.
                    for d in missing.into_iter().rev() {
                        stack.push(Frame { key: d, deps: None });
                    }
                }
                Some(deps) => {
                    // Every dependency now has a value: derive this key.
                    let mut pairs: Vec<(Bytes, Bytes)> = Vec::with_capacity(deps.len());
                    for d in deps {
                        let v = values.get(&d).cloned().ok_or_else(|| {
                            engine_bug(format!(
                                "derived type {ty} v{version}: dependency missing after expansion"
                            ))
                        })?;
                        pairs.push((d, v));
                    }
                    let v = self
                        .derive_one(t.as_ref(), version, &key, &pairs, &holder, &mut contended)
                        .await?;
                    expanding.remove(&key);
                    backlog.add(-1);
                    values.insert(key, v);
                    stack.pop();
                }
            }
        }
        values
            .remove(&root)
            .ok_or_else(|| engine_bug(format!("derived type {ty} v{version}: no value for root")))
    }

    /// Derive one key whose dependency values are known, under a best-effort lease, and write
    /// it back (`ON CONFLICT DO NOTHING`).
    async fn derive_one(
        &self,
        t: &dyn LazyType,
        version: u32,
        key: &Bytes,
        deps: &[(Bytes, Bytes)],
        holder: &str,
        contended: &mut bool,
    ) -> Result<Bytes> {
        let ty = t.name();
        let dk = Self::dkey(ty, version, key);
        if *contended && let Some(v) = read_stored(&self.objects, &dk).await? {
            // Another worker is ahead of us on the same chain.
            return Ok(v);
        }
        let lease = match lease::acquire_or_wait(
            &self.objects,
            self.clock.as_ref(),
            &dk,
            holder,
            self.cfg.lease_ttl,
        )
        .await?
        {
            LeaseOutcome::Acquired(l) => Some(l),
            LeaseOutcome::Stored(v) => {
                *contended = true;
                return Ok(v);
            }
            LeaseOutcome::Proceed => {
                *contended = true;
                None
            }
        };
        let result = self.derive_and_store(t, dk, deps).await;
        if let Some(l) = lease {
            l.release(&self.objects).await;
        }
        result
    }

    async fn derive_and_store(
        &self,
        t: &dyn LazyType,
        dk: DerivedKey,
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        let start = Instant::now();
        let value = t.derive(&self.ctx, &dk.key, deps).await?;
        if let Some(m) = self.metrics() {
            m.derive_duration(t.name(), DeriveMode::Lazy, start.elapsed());
        }
        // Content-bearing records are keyed by their source object id (SPEC 9.1, 13.5): a
        // content-bearing lazy type whose key is an object id records it as the source.
        let source_object = if t.content_bearing() {
            ObjectId::from_bytes(&dk.key).ok()
        } else {
            None
        };
        let row = DerivedRow {
            key: dk,
            value: Some(value.clone()),
            storage_key: None,
            source_object,
            at: self.clock.now(),
        };
        self.objects.storage().meta.put_derived(vec![row]).await?;
        Ok(value)
    }
}

#[async_trait]
impl EagerHook for DerivedEngine {
    /// SPEC 9.1, 5.3: every registered eager type, in registration order; rows concatenated.
    async fn derive_eager(&self, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        let metrics = self.metrics();
        let mut rows = Vec::new();
        for t in self.registry.eager_types() {
            let start = Instant::now();
            let out = t.derive(&self.ctx, input).await?;
            if let Some(m) = &metrics {
                m.derive_duration(t.name(), DeriveMode::Eager, start.elapsed());
            }
            rows.extend(out);
        }
        Ok(rows)
    }

    /// SPEC 4.5: every registered eager `(name, version)`, descriptors included.
    fn required_eager(&self) -> Vec<(String, u32)> {
        self.registry.eager_pairs()
    }
}
