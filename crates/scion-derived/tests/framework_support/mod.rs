//! Shared helpers for the P14 framework tests: in-memory engines and test-only derived types.
#![allow(dead_code)]

pub mod offstack;

use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ChangeId, Clock, FaultRegistry, Kind, ManualClock, ObjectId, Result, ScionError};
use scion_derived::{
    DeriveConfig, DeriveCtx, DeriveMetrics, DeriveMode, DerivedEngine, DerivedType, EagerType,
    LazyType, Registry,
};
use scion_history::EagerInput;
use scion_object::{Canonical, Changeset, Extensions, Meta, Timestamp, Tree};
use scion_store::{
    DerivedKey, DerivedRow, MemBlobStore, MemHandles, MemMetaStore, MemOptions, MetaStore,
    ObjectRepo, ObjectRepoConfig, Storage,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// A fresh in-memory storage bundle.
pub fn storage() -> (Storage, MemHandles) {
    Storage::in_memory(MemOptions::default())
}

/// A fresh in-memory storage bundle whose metadata calls run on `rt` (see [`offstack`]).
pub fn offstack_storage(rt: tokio::runtime::Handle) -> (Storage, MemHandles) {
    let mem = Arc::new(MemMetaStore::new(MemOptions::default()));
    let blobs = Arc::new(MemBlobStore::new());
    let meta: Arc<dyn MetaStore> = Arc::new(offstack::OffStackMeta::new(mem.clone(), rt));
    let storage = Storage::new(meta, blobs.clone(), FaultRegistry::new());
    (storage, MemHandles { meta: mem, blobs })
}

/// An object facade over `storage`.
pub fn repo(storage: &Storage) -> ObjectRepo {
    ObjectRepo::new(storage.clone(), ObjectRepoConfig::default())
}

/// A deterministic clock.
pub fn clock() -> Arc<ManualClock> {
    ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
}

/// An engine over `storage` with `registry` and lease holder `holder`.
pub fn engine(storage: &Storage, registry: Registry, holder: &str) -> Arc<DerivedEngine> {
    engine_with(storage, registry, holder, clock())
}

/// [`engine`] with an explicit clock.
pub fn engine_with(
    storage: &Storage,
    registry: Registry,
    holder: &str,
    clock: Arc<dyn Clock>,
) -> Arc<DerivedEngine> {
    let cfg = DeriveConfig {
        holder_id: holder.to_owned(),
        ..DeriveConfig::default()
    };
    DerivedEngine::new(repo(storage), registry, cfg, clock)
}

/// The key of chain element `n`.
pub fn ck(n: u64) -> Bytes {
    Bytes::copy_from_slice(&n.to_be_bytes())
}

fn parse_key(key: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = key
        .try_into()
        .map_err(|_| ScionError::invalid("bad_key", "test key must be 8 bytes"))?;
    Ok(u64::from_be_bytes(arr))
}

/// The value function of [`ChainType`]: `H(version || key || dep values...)`.
pub fn chain_value(version: u32, key: &[u8], deps: &[&[u8]]) -> Bytes {
    let mut buf = Vec::new();
    buf.extend_from_slice(&version.to_be_bytes());
    buf.extend_from_slice(key);
    for d in deps {
        buf.extend_from_slice(d);
    }
    Bytes::copy_from_slice(ObjectId::hash_blob(&buf).as_bytes())
}

/// The expected values of a chain `0..=n` computed directly (independent of the engine).
pub fn expected_chain(version: u32, n: u64) -> Vec<Bytes> {
    let mut out: Vec<Bytes> = Vec::with_capacity(n as usize + 1);
    for i in 0..=n {
        let k = ck(i);
        let v = match out.last() {
            Some(prev) => chain_value(version, &k, &[prev]),
            None => chain_value(version, &k, &[]),
        };
        out.push(v);
    }
    out
}

/// A test-only lazy type: key `n` (8 bytes BE) depends on `n - 1`; key 0 has no dependency.
pub struct ChainType {
    pub name: &'static str,
    pub version: u32,
    pub content_bearing: bool,
    /// Sleep inside `derive` (to force overlap between concurrent workers).
    pub delay: Option<Duration>,
    pub derive_calls: AtomicU64,
    pub deps_calls: AtomicU64,
}

impl ChainType {
    pub fn new(name: &'static str, version: u32) -> Self {
        ChainType {
            name,
            version,
            content_bearing: false,
            delay: None,
            derive_calls: AtomicU64::new(0),
            deps_calls: AtomicU64::new(0),
        }
    }

    pub fn derives(&self) -> u64 {
        self.derive_calls.load(Ordering::SeqCst)
    }
}

impl DerivedType for ChainType {
    fn name(&self) -> &'static str {
        self.name
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }
    fn content_bearing(&self) -> bool {
        self.content_bearing
    }
}

#[async_trait]
impl LazyType for ChainType {
    async fn deps(&self, _cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>> {
        self.deps_calls.fetch_add(1, Ordering::SeqCst);
        let n = parse_key(key)?;
        Ok(if n == 0 { vec![] } else { vec![ck(n - 1)] })
    }

    async fn derive(
        &self,
        _cx: &dyn DeriveCtx,
        key: &[u8],
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        self.derive_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        let n = parse_key(key)?;
        let expect: Vec<Bytes> = if n == 0 { vec![] } else { vec![ck(n - 1)] };
        let got: Vec<Bytes> = deps.iter().map(|(k, _)| k.clone()).collect();
        if got != expect {
            return Err(ScionError::invalid(
                "bad_deps",
                "unexpected dependency keys",
            ));
        }
        let dv: Vec<&[u8]> = deps.iter().map(|(_, v)| v.as_ref()).collect();
        Ok(chain_value(self.version, key, &dv))
    }
}

/// A test-only lazy type over a DAG: key `n` depends on `n - 1` and `n - 2` (when they exist).
pub struct FibType;

impl DerivedType for FibType {
    fn name(&self) -> &'static str {
        "test_fib"
    }
    fn version(&self) -> u32 {
        1
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }
    fn content_bearing(&self) -> bool {
        false
    }
}

/// Fibonacci numbers modulo 2^64, as the [`FibType`] oracle.
pub fn fib(n: u64) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        let c = a.wrapping_add(b);
        a = b;
        b = c;
    }
    a
}

#[async_trait]
impl LazyType for FibType {
    async fn deps(&self, _cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>> {
        let n = parse_key(key)?;
        Ok(match n {
            0 | 1 => vec![],
            _ => vec![ck(n - 1), ck(n - 2)],
        })
    }

    async fn derive(
        &self,
        _cx: &dyn DeriveCtx,
        key: &[u8],
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        let n = parse_key(key)?;
        let v = match n {
            0 => 0,
            1 => 1,
            _ => {
                let mut s = 0u64;
                for (_, v) in deps {
                    s = s.wrapping_add(parse_key(v)?);
                }
                s
            }
        };
        Ok(ck(v))
    }
}

/// A broken lazy type whose dependencies form a cycle of length 3.
pub struct CycleType;

impl DerivedType for CycleType {
    fn name(&self) -> &'static str {
        "test_cycle"
    }
    fn version(&self) -> u32 {
        1
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }
    fn content_bearing(&self) -> bool {
        false
    }
}

#[async_trait]
impl LazyType for CycleType {
    async fn deps(&self, _cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>> {
        let n = parse_key(key)?;
        Ok(vec![ck((n + 1) % 3)])
    }

    async fn derive(
        &self,
        _cx: &dyn DeriveCtx,
        key: &[u8],
        _deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        Ok(Bytes::copy_from_slice(key))
    }
}

/// A test-only eager type emitting one row keyed by the changeset id (value: its name), plus
/// `extra` rows keyed `<name>:<i>`.
pub struct TagEager {
    pub name: &'static str,
    pub version: u32,
    pub extra: usize,
    pub content_bearing: bool,
}

impl DerivedType for TagEager {
    fn name(&self) -> &'static str {
        self.name
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Eager
    }
    fn content_bearing(&self) -> bool {
        self.content_bearing
    }
}

#[async_trait]
impl EagerType for TagEager {
    async fn derive(&self, _cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        let at = SystemTime::UNIX_EPOCH;
        let mut rows = vec![DerivedRow {
            key: DerivedKey {
                ty: self.name.to_owned(),
                version: self.version,
                key: Bytes::copy_from_slice(input.id.as_bytes()),
            },
            value: Some(Bytes::from_static(self.name.as_bytes())),
            storage_key: None,
            source_object: None,
            at,
        }];
        for i in 0..self.extra {
            rows.push(DerivedRow {
                key: DerivedKey {
                    ty: self.name.to_owned(),
                    version: self.version,
                    key: Bytes::from(format!("{}:{i}", self.name)),
                },
                value: Some(Bytes::from(vec![i as u8])),
                storage_key: None,
                source_object: None,
                at,
            });
        }
        Ok(rows)
    }
}

/// A sample (unstored) changeset and Meta for building an [`EagerInput`].
pub fn sample_changeset() -> (ObjectId, Changeset, Meta) {
    let meta = Meta {
        author: b"github:alice".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"github:alice".to_vec(),
        message: b"sample".to_vec(),
        extensions: Extensions::new(),
    };
    let cs = Changeset {
        parents: vec![],
        root_tree: Tree::empty().id(),
        meta: meta.id(),
        change_id: ChangeId::random(),
        delta: vec![],
        extensions: Extensions::new(),
    };
    let id = ObjectId::hash_encoded(Kind::Changeset, &cs.encode());
    (id, cs, meta)
}

/// Recording metrics hook.
#[derive(Default)]
pub struct RecMetrics {
    pub durations: parking_lot::Mutex<Vec<(String, DeriveMode)>>,
    pub backlog: parking_lot::Mutex<Vec<(String, i64)>>,
}

impl RecMetrics {
    pub fn backlog_sum(&self, ty: &str) -> i64 {
        self.backlog
            .lock()
            .iter()
            .filter(|(t, _)| t == ty)
            .map(|(_, d)| d)
            .sum()
    }
    pub fn backlog_peak(&self, ty: &str) -> i64 {
        let mut cur = 0;
        let mut peak = 0;
        for (t, d) in self.backlog.lock().iter() {
            if t == ty {
                cur += d;
                peak = peak.max(cur);
            }
        }
        peak
    }
    pub fn durations_of(&self, ty: &str, mode: DeriveMode) -> usize {
        self.durations
            .lock()
            .iter()
            .filter(|(t, m)| t == ty && *m == mode)
            .count()
    }
}

impl DeriveMetrics for RecMetrics {
    fn derive_duration(&self, ty: &str, mode: DeriveMode, _d: Duration) {
        self.durations.lock().push((ty.to_owned(), mode));
    }
    fn backlog(&self, ty: &str, delta: i64) {
        self.backlog.lock().push((ty.to_owned(), delta));
    }
}
