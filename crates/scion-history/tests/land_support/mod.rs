//! Shared helpers for the P20 refs/land integration tests (SPEC 19.5, 19.18).
//!
//! - [`FakeRebaser`]: re-applies `C.delta` onto `onto` via `create_internal` (MERGE_CONFLICT when
//!   `onto` changed a delta path differently or apply_delta fails, EMPTY_CHANGE when every entry
//!   is already present), so the land tests do not depend on the real SPEC 18.3 rebase.
//! - [`ContendedMeta`]: a `MetaStore` decorator over `MemMetaStore` that can force `cas_ref`
//!   version mismatches without changing the ref (lost CAS races).
#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::{
    ChangeId, ErrorCode, ErrorPayload, FaultRegistry, ManualClock, Mode, ObjectId, Principal,
    Result, ScionError, subcode,
};
use scion_history::{
    AllowAll, CreateRequest, History, HistoryConfig, LandAttemptOutcome, LandAuthorizer,
    LandAuthorizerSource, LandConfig, LandObserver, LandRequest, LandResult, MetaFields,
    MetaSource, NoEager, Rebaser,
};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RefName, RepoPath, Timestamp};
use scion_store::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------------------------
// Contended metadata store
// ---------------------------------------------------------------------------------------------

/// Delegates to `MemMetaStore`; `cas_ref` reports `VersionMismatch` (with the real current row,
/// no effect) while `forced_mismatches > 0` (`u32::MAX` = always).
pub struct ContendedMeta {
    pub inner: Arc<MemMetaStore>,
    pub forced_mismatches: AtomicU32,
    pub cas_calls: AtomicU32,
}

impl ContendedMeta {
    pub fn force_mismatches(&self, n: u32) {
        self.forced_mismatches.store(n, Ordering::SeqCst);
    }
}

#[async_trait]
impl MetaStore for ContendedMeta {
    fn repo_id(&self) -> RepoId {
        self.inner.repo_id()
    }
    async fn ping(&self) -> Result<()> {
        self.inner.ping().await
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        self.inner.get_objects(ids, c).await
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        self.inner.insert_object(row).await
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        self.inner.record_upload(id, principal, at).await
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        self.inner.has_upload(id, principal).await
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        self.inner.object_paths(id, limit).await
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        self.inner.get_changesets(ids, c).await
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        self.inner.changesets_by_prefix(hex_prefix, limit).await
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        self.inner.children(parent).await
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        self.inner.has_root_changeset().await
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        self.inner.commit_changeset(b).await
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        self.inner.get_ref(name).await
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        self.inner.list_refs().await
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        self.cas_calls.fetch_add(1, Ordering::SeqCst);
        let forced = self
            .forced_mismatches
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| match n {
                0 => None,
                u32::MAX => Some(u32::MAX),
                n => Some(n - 1),
            })
            .is_ok();
        if forced {
            let current = self.inner.get_ref(&cas.name).await?;
            return Ok(CasOutcome::VersionMismatch { current });
        }
        self.inner.cas_ref(cas).await
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        self.inner.ref_log(name, from_version, limit).await
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        self.inner.get_landed(name, change_id).await
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        self.inner.get_derived(k, c).await
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        self.inner.put_derived(rows).await
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        self.inner.delete_derived_by_source(types, source).await
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        self.inner.try_lease(k, holder, ttl, now).await
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        self.inner.release_lease(k, holder).await
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        self.inner.redact(t).await
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        self.inner.unredact(ids, audit).await
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        self.inner.mark_purged(ids, audit).await
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        self.inner.redaction_epoch().await
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        self.inner.list_redactions(since_epoch).await
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        self.inner.due_purges(now, limit).await
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        self.inner.append_audit(row).await
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        self.inner.audit_log(limit).await
    }
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

pub struct Env {
    pub h: Arc<History>,
    pub mem: Arc<MemMetaStore>,
    pub contended: Arc<ContendedMeta>,
    pub faults: Arc<FaultRegistry>,
    pub counters: Arc<Counters>,
    pub clock: Arc<ManualClock>,
    pub observer: Arc<RecordingObserver>,
    /// The bootstrap root changeset (`main@1`).
    pub root: ObjectId,
}

/// Fast land configuration for tests: tiny backoff, seeded RNG.
pub fn fast_land(max_attempts: u32) -> HistoryConfig {
    HistoryConfig {
        land: LandConfig {
            max_attempts,
            backoff_base: Duration::from_micros(200),
            backoff_max: Duration::from_millis(5),
            rng_seed: Some(42),
        },
        ..HistoryConfig::default()
    }
}

pub async fn env_with(cfg: HistoryConfig) -> Env {
    let mem = Arc::new(MemMetaStore::new(MemOptions::default()));
    let contended = Arc::new(ContendedMeta {
        inner: mem.clone(),
        forced_mismatches: AtomicU32::new(0),
        cas_calls: AtomicU32::new(0),
    });
    let faults = FaultRegistry::new();
    let storage = Storage::new(
        contended.clone(),
        Arc::new(MemBlobStore::new()),
        faults.clone(),
    );
    let counters = storage.counters.clone();
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    let observer = Arc::new(RecordingObserver::default());
    let h = History::new(objects, cfg, Arc::new(NoEager), clock.clone())
        .with_land_observer(observer.clone());
    let root = h.bootstrap().await.expect("bootstrap");
    Env {
        h: Arc::new(h),
        mem,
        contended,
        faults,
        counters,
        clock,
        observer,
        root,
    }
}

pub async fn env() -> Env {
    env_with(fast_land(32)).await
}

impl Env {
    pub fn rebaser(&self) -> FakeRebaser {
        FakeRebaser { h: self.h.clone() }
    }

    /// Land `head` onto `main` as alice with every permission and the fake rebaser.
    pub async fn land(&self, head: ObjectId) -> Result<LandResult> {
        self.land_on(&main(), head, false).await
    }

    pub async fn land_on(
        &self,
        r: &RefName,
        head: ObjectId,
        allow_empty: bool,
    ) -> Result<LandResult> {
        self.h
            .land_with(
                LandRequest {
                    ref_name: r.clone(),
                    head,
                    allow_empty,
                },
                &alice(),
                &AllowAll,
                &self.rebaser(),
            )
            .await
    }

    pub async fn main_row(&self) -> RefRow {
        self.h.resolve_ref(&main(), None).await.expect("main")
    }

    /// Upload content and commit it at `path` on top of `parent`.
    pub async fn commit(
        &self,
        parent: ObjectId,
        path: &str,
        content: &str,
        change: u8,
    ) -> ObjectId {
        let delta = vec![put(&self.h, path, content).await];
        commit_delta(&self.h, vec![parent], delta, cid(change)).await
    }

    /// The content of `path` in changeset `cs` (None when absent).
    pub async fn read(&self, cs: &ObjectId, path: &str) -> Option<Vec<u8>> {
        read_file(&self.h, cs, path).await
    }

    /// Every ref_log row of `name`.
    pub async fn log(&self, name: &RefName) -> Vec<RefLogRow> {
        self.h.ref_log(name, 0, usize::MAX).await.expect("ref_log")
    }
}

pub fn main() -> RefName {
    RefName::main()
}

pub fn rn(s: &str) -> RefName {
    RefName::parse(s).expect("ref name")
}

pub fn cid(n: u8) -> ChangeId {
    ChangeId([n; 16])
}

pub fn cid16(n: u16) -> ChangeId {
    let mut b = [0xA5u8; 16];
    b[..2].copy_from_slice(&n.to_be_bytes());
    ChangeId(b)
}

pub fn p(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).expect("valid test path")
}

pub fn alice() -> Principal {
    Principal::GitHub {
        login: "alice".to_owned(),
        id: Some(7),
    }
}

pub fn fields(message: &str) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

/// Upload `content` and return a Put of it at `path`.
pub async fn put(h: &History, path: &str, content: &str) -> DeltaEntry {
    let (id, _) = h
        .objects()
        .put_blob(Bytes::from(content.as_bytes().to_vec()))
        .await
        .expect("upload");
    DeltaEntry {
        path: p(path),
        op: DeltaOp::Put {
            mode: Mode::File,
            id,
            size: content.len() as u64,
        },
    }
}

pub async fn commit_delta(
    h: &History,
    parents: Vec<ObjectId>,
    delta: Vec<DeltaEntry>,
    change_id: ChangeId,
) -> ObjectId {
    h.create_changeset(
        CreateRequest {
            parents,
            delta,
            meta: fields("test change"),
            change_id,
            expected_root_tree: None,
            import_mode: false,
        },
        &alice(),
        &AllowAll,
    )
    .await
    .expect("create_changeset")
    .changeset_id
}

pub async fn read_file(h: &History, cs: &ObjectId, path: &str) -> Option<Vec<u8>> {
    let rec = h.changeset(cs).await.expect("changeset");
    let entry = h
        .lookup(&rec.changeset.root_tree, &p(path))
        .await
        .expect("lookup")?;
    Some(h.objects().get(&entry.id).await.expect("blob").to_vec())
}

pub fn fsck_opts() -> FsckOptions {
    FsckOptions {
        required_eager: vec![("commit_graph".to_owned(), 1), ("path_index".to_owned(), 1)],
        verify_bytes: true,
    }
}

pub async fn assert_fsck_clean(h: &History) {
    let v = scion_store::fsck(h.objects(), &fsck_opts())
        .await
        .expect("fsck");
    assert!(v.is_empty(), "fsck violations: {v:?}");
}

// ---------------------------------------------------------------------------------------------
// Fake rebaser
// ---------------------------------------------------------------------------------------------

pub fn merge_conflict(msg: String) -> ScionError {
    ScionError::new(ErrorCode::MergeConflict, msg)
        .with_payload(ErrorPayload::MergeConflict(Box::default()))
}

/// Re-applies `C.delta` onto `onto` via `create_internal` (see the module docs).
pub struct FakeRebaser {
    pub h: Arc<History>,
}

#[async_trait]
impl Rebaser for FakeRebaser {
    async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        let rec = self.h.changeset(c).await?;
        let [parent] = rec.changeset.parents[..] else {
            return Err(ScionError::invalid(subcode::MULTI_PARENT, "multi-parent"));
        };
        if *onto == parent {
            return Ok(*c);
        }
        let base_root = self.h.changeset(&parent).await?.changeset.root_tree;
        let onto_root = self.h.changeset(onto).await?.changeset.root_tree;
        let mut delta = Vec::new();
        for e in &rec.changeset.delta {
            let key = |t: Option<scion_object::TreeEntry>| t.map(|t| (t.mode, t.id));
            let base = key(self.h.lookup(&base_root, &e.path).await?);
            let theirs = key(self.h.lookup(&onto_root, &e.path).await?);
            let want = match e.op {
                DeltaOp::Put { mode, id, .. } => Some((mode, id)),
                DeltaOp::Delete => None,
            };
            if theirs == want {
                continue;
            }
            if theirs != base {
                return Err(merge_conflict(format!("{c}: a delta path changed on onto")));
            }
            delta.push(e.clone());
        }
        if delta.is_empty() {
            return Err(ScionError::new(
                ErrorCode::EmptyChange,
                "empty after rebase",
            ));
        }
        if let Err(e) = self.h.apply_delta(&onto_root, &delta).await {
            return Err(merge_conflict(format!("{c}: apply_delta failed: {e}")));
        }
        let created = self
            .h
            .create_internal(
                vec![*onto],
                delta,
                MetaSource::Existing(rec.changeset.meta),
                rec.changeset.change_id,
                who,
            )
            .await?;
        Ok(created.changeset_id)
    }
}

// ---------------------------------------------------------------------------------------------
// Authorizers and observers
// ---------------------------------------------------------------------------------------------

/// Configurable authorizer.
#[derive(Clone, Copy, Debug)]
pub struct Authz {
    pub update: bool,
    pub force: bool,
    pub write: bool,
}

impl Authz {
    pub const ALL: Authz = Authz {
        update: true,
        force: true,
        write: true,
    };
}

impl LandAuthorizer for Authz {
    fn can_update(&self, _r: &RefName) -> bool {
        self.update
    }
    fn can_force(&self, _r: &RefName) -> bool {
        self.force
    }
    fn can_write(&self, _p: &RepoPath) -> bool {
        self.write
    }
}

impl LandAuthorizerSource for Authz {
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        Arc::new(*self)
    }
}

/// Returns `first` for the first `n` calls of `current()`, then `then` (a policy reload).
pub struct FlipSource {
    pub n: u32,
    pub first: Authz,
    pub then: Authz,
    pub calls: AtomicU32,
}

impl LandAuthorizerSource for FlipSource {
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        Arc::new(if i < self.n { self.first } else { self.then })
    }
}

/// Records every land attempt.
#[derive(Default)]
pub struct RecordingObserver {
    pub attempts: Mutex<Vec<(RefName, u32, LandAttemptOutcome)>>,
}

impl RecordingObserver {
    pub fn take(&self) -> Vec<(u32, LandAttemptOutcome)> {
        std::mem::take(&mut *self.attempts.lock())
            .into_iter()
            .map(|(_, a, o)| (a, o))
            .collect()
    }
}

impl LandObserver for RecordingObserver {
    fn attempt(
        &self,
        ref_name: &RefName,
        attempt: u32,
        outcome: &LandAttemptOutcome,
        _elapsed: Duration,
    ) {
        self.attempts
            .lock()
            .push((ref_name.clone(), attempt, outcome.clone()));
    }
}
