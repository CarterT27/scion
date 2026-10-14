//! Faithful hermetic in-memory [`MetaStore`] (PLAN 3.2, `docs/contracts/storage-semantics.md`).
//!
//! All state lives in `im` persistent maps behind ONE `parking_lot::Mutex`. Every trait method
//! takes the lock exactly once and works on a cheap persistent copy of the tables, which replaces
//! the committed state only when the method succeeds; a failed method therefore never leaves a
//! partial effect, and the store is strictly serializable.

use crate::meta::MetaStore;
use crate::types::*;
use async_trait::async_trait;
use bytes::Bytes;
use im::{OrdMap, OrdSet, Vector};
use parking_lot::Mutex;
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_object::{RefName, RepoPath};
use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, SystemTime};

/// Options of the in-memory metadata store.
#[derive(Clone, Debug)]
pub struct MemOptions {
    /// Repository name (`repos.name`).
    pub repo_name: String,
    /// How many commits stale reads lag behind (0: stale == strong).
    pub stale_lag_commits: usize,
    /// Every Nth transaction fails UNAVAILABLE/txn_retry with no effect.
    pub fail_txn_every: Option<u32>,
    /// Seed for any internal randomness.
    pub seed: u64,
}

impl Default for MemOptions {
    fn default() -> Self {
        MemOptions {
            repo_name: "test".to_owned(),
            stale_lag_commits: 0,
            fail_txn_every: None,
            seed: 0,
        }
    }
}

/// `(type, type_version, key)`: the `derived` / `derive_leases` primary key.
type DKey = (String, u32, Bytes);

fn dkey(k: &DerivedKey) -> DKey {
    (k.ty.clone(), k.version, k.key.clone())
}

/// One committed state of every table (SPEC 5.2 plus the PLAN 6.8 audit log).
#[derive(Clone, Debug, Default)]
struct Tables {
    objects: OrdMap<ObjectId, ObjectRow>,
    uploads: OrdMap<(ObjectId, String), SystemTime>,
    object_paths: OrdMap<ObjectId, OrdMap<RepoPath, ObjectPathRow>>,
    changesets: OrdMap<ObjectId, ChangesetRow>,
    root_changesets: usize,
    children: OrdMap<ObjectId, OrdSet<ObjectId>>,
    refs: OrdMap<RefName, RefRow>,
    ref_log: OrdMap<RefName, OrdMap<u64, RefLogRow>>,
    landed: OrdMap<(RefName, ChangeId), LandedRow>,
    derived: OrdMap<DKey, DerivedRow>,
    leases: OrdMap<DKey, (String, SystemTime)>,
    redactions: OrdMap<ObjectId, RedactionRow>,
    redaction_epoch: u64,
    audit: Vector<AuditRow>,
}

/// Mutable state guarded by the single mutex.
#[derive(Debug)]
struct MemState {
    /// The latest committed tables.
    cur: Tables,
    /// The last `lag + 1` committed states, oldest first; the back is `cur`.
    ring: VecDeque<Tables>,
    /// Stale-read lag in commits.
    lag: usize,
    /// Every Nth method call fails (None: never).
    fail_every: Option<u32>,
    /// Method calls so far (for `fail_every`).
    calls: u64,
}

impl MemState {
    fn commit(&mut self, next: Tables) {
        self.cur = next.clone();
        self.ring.push_back(next);
        self.trim();
    }

    fn trim(&mut self) {
        while self.ring.len() > self.lag + 1 {
            self.ring.pop_front();
        }
    }

    fn snapshot(&self, c: Consistency) -> &Tables {
        match c {
            Consistency::Strong => &self.cur,
            Consistency::Stale => self.ring.front().unwrap_or(&self.cur),
        }
    }

    /// Count one method call and apply the forced-failure schedule.
    fn begin(&mut self) -> Result<()> {
        self.calls += 1;
        if let Some(n) = self.fail_every
            && n > 0
            && self.calls.is_multiple_of(u64::from(n))
        {
            return Err(ScionError::unavailable(
                subcode::TXN_RETRY,
                "forced transaction failure (fail_txn_every)",
            ));
        }
        Ok(())
    }
}

/// `repo_id = i64::from_be_bytes(BLAKE3(repo_name)[0..8]) & i64::MAX`
/// (`docs/contracts/storage-semantics.md` 1.5).
fn compute_repo_id(name: &str) -> RepoId {
    let h = blake3::hash(name.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&h.as_bytes()[..8]);
    RepoId(i64::from_be_bytes(b) & i64::MAX)
}

/// One `parking_lot::Mutex` over `im` persistent maps; a ring of the last K committed snapshots
/// serves [`Consistency::Stale`]. Each trait method is one atomic step (strictly
/// serializable).
#[derive(Debug)]
pub struct MemMetaStore {
    opts: MemOptions,
    repo_id: RepoId,
    state: Mutex<MemState>,
}

impl MemMetaStore {
    /// An empty store.
    pub fn new(opts: MemOptions) -> Self {
        let mut ring = VecDeque::new();
        ring.push_back(Tables::default());
        let state = MemState {
            cur: Tables::default(),
            ring,
            lag: opts.stale_lag_commits,
            fail_every: opts.fail_txn_every,
            calls: 0,
        };
        MemMetaStore {
            repo_id: compute_repo_id(&opts.repo_name),
            opts,
            state: Mutex::new(state),
        }
    }

    /// The options this store was created with.
    pub fn options(&self) -> &MemOptions {
        &self.opts
    }

    /// Change the stale-read lag (SPEC 5.4 tests).
    pub fn set_stale_lag(&self, commits: usize) {
        let mut st = self.state.lock();
        st.lag = commits;
        st.trim();
    }

    /// Advance the stale view to the latest committed snapshot.
    pub fn advance_stale_view(&self) {
        let mut st = self.state.lock();
        let cur = st.cur.clone();
        st.ring.clear();
        st.ring.push_back(cur);
    }

    /// Change the forced-failure schedule (`None` disables it); the call counter restarts.
    pub fn set_fail_txn_every(&self, every: Option<u32>) {
        let mut st = self.state.lock();
        st.fail_every = every;
        st.calls = 0;
    }

    /// Row counts per table.
    pub fn table_counts(&self) -> TableCounts {
        let st = self.state.lock();
        let t = &st.cur;
        TableCounts {
            objects: t.objects.len(),
            changesets: t.changesets.len(),
            refs: t.refs.len(),
            ref_log: t.ref_log.values().map(OrdMap::len).sum(),
            landed: t.landed.len(),
            derived: t.derived.len(),
            object_paths: t.object_paths.values().map(OrdMap::len).sum(),
            redactions: t.redactions.len(),
        }
    }

    /// Every objects row.
    pub fn all_objects(&self) -> Vec<ObjectRow> {
        self.state.lock().cur.objects.values().cloned().collect()
    }

    /// Every changesets row.
    pub fn all_changesets(&self) -> Vec<ChangesetRow> {
        self.state.lock().cur.changesets.values().cloned().collect()
    }

    /// Test support (fsck tests): overwrite or insert an objects row, bypassing every rule.
    pub fn put_object_row_raw(&self, row: ObjectRow) {
        self.tamper(|t| {
            t.objects.insert(row.id, row);
        });
    }

    /// Test support: delete an objects row, bypassing every rule.
    pub fn remove_object_row_raw(&self, id: &ObjectId) -> bool {
        let mut removed = false;
        self.tamper(|t| removed = t.objects.remove(id).is_some());
        removed
    }

    /// Test support: overwrite or insert a changesets row, bypassing every rule.
    pub fn put_changeset_row_raw(&self, row: ChangesetRow) {
        self.tamper(|t| {
            t.changesets.insert(row.id, row);
            t.root_changesets = t.changesets.values().filter(|r| r.generation == 1).count();
        });
    }

    /// Test support: delete a derived row, bypassing every rule.
    pub fn remove_derived_raw(&self, k: &DerivedKey) -> bool {
        let mut removed = false;
        self.tamper(|t| removed = t.derived.remove(&dkey(k)).is_some());
        removed
    }

    /// Test support: delete the `object_paths` row `(object, path)`, bypassing every rule.
    pub fn remove_object_path_raw(&self, object: &ObjectId, path: &RepoPath) -> bool {
        let mut removed = false;
        self.tamper(|t| {
            if let Some(mut m) = t.object_paths.get(object).cloned() {
                removed = m.remove(path).is_some();
                if m.is_empty() {
                    t.object_paths.remove(object);
                } else {
                    t.object_paths.insert(*object, m);
                }
            }
        });
        removed
    }

    /// Test support: overwrite or insert a refs row, bypassing every rule.
    pub fn put_ref_row_raw(&self, row: RefRow) {
        self.tamper(|t| {
            t.refs.insert(row.name.clone(), row);
        });
    }

    fn tamper(&self, f: impl FnOnce(&mut Tables)) {
        let mut st = self.state.lock();
        let mut next = st.cur.clone();
        f(&mut next);
        st.commit(next);
    }

    /// One read-only transaction against the snapshot chosen by `c`.
    fn read<T>(&self, c: Consistency, f: impl FnOnce(&Tables) -> Result<T>) -> Result<T> {
        let mut st = self.state.lock();
        st.begin()?;
        f(st.snapshot(c))
    }

    /// One read-write transaction. `f` returns the result and whether it changed anything; the
    /// working copy replaces the committed state only on success.
    fn write<T>(&self, f: impl FnOnce(&mut Tables) -> Result<(T, bool)>) -> Result<T> {
        let mut st = self.state.lock();
        st.begin()?;
        let mut next = st.cur.clone();
        let (out, changed) = f(&mut next)?;
        if changed {
            st.commit(next);
        }
        Ok(out)
    }
}

fn invalid(sub: &str, msg: impl Into<String>) -> ScionError {
    ScionError::invalid(sub, msg)
}

fn valid_hex_prefix(p: &str) -> bool {
    p.len() <= 64
        && p.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn prefix_floor(p: &str) -> Option<ObjectId> {
    let mut padded = p.to_owned();
    while padded.len() < 64 {
        padded.push('0');
    }
    let mut digest = [0u8; 32];
    hex::decode_to_slice(&padded, &mut digest).ok()?;
    Some(ObjectId::new(Kind::Changeset, digest))
}

#[async_trait]
impl MetaStore for MemMetaStore {
    fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    async fn ping(&self) -> Result<()> {
        self.read(Consistency::Strong, |_| Ok(()))
    }

    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        self.read(c, |t| {
            Ok(ids.iter().map(|id| t.objects.get(id).cloned()).collect())
        })
    }

    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        self.write(|t| {
            if t.objects.contains_key(&row.id) {
                return Ok((InsertOutcome::AlreadyExists, false));
            }
            t.objects.insert(row.id, row);
            Ok((InsertOutcome::Inserted, true))
        })
    }

    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        self.write(|t| {
            let k = (*id, principal.to_owned());
            if t.uploads.contains_key(&k) {
                return Ok(((), false));
            }
            t.uploads.insert(k, at);
            Ok(((), true))
        })
    }

    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        self.read(Consistency::Strong, |t| {
            Ok(t.uploads.contains_key(&(*id, principal.to_owned())))
        })
    }

    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        self.read(Consistency::Strong, |t| {
            Ok(t.object_paths
                .get(id)
                .map(|m| m.values().take(limit).cloned().collect())
                .unwrap_or_default())
        })
    }

    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        self.read(c, |t| {
            Ok(ids.iter().map(|id| t.changesets.get(id).cloned()).collect())
        })
    }

    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        if !valid_hex_prefix(hex_prefix) {
            return Err(invalid(
                subcode::BAD_ID_TEXT,
                "changeset prefix must be at most 64 lowercase hex digits",
            ));
        }
        let floor = prefix_floor(hex_prefix)
            .ok_or_else(|| invalid(subcode::BAD_ID_TEXT, "bad changeset prefix"))?;
        self.read(Consistency::Strong, |t| {
            Ok(t.changesets
                .range(floor..)
                .map(|(id, _)| *id)
                .take_while(|id| id.hex().starts_with(hex_prefix))
                .take(limit)
                .collect())
        })
    }

    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        self.read(Consistency::Strong, |t| {
            Ok(t.children
                .get(parent)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default())
        })
    }

    async fn has_root_changeset(&self) -> Result<bool> {
        self.read(Consistency::Strong, |t| Ok(t.root_changesets > 0))
    }

    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        self.write(|t| {
            let mut changed = false;
            let child = b.row.id;
            let outcome = if t.changesets.contains_key(&child) {
                InsertOutcome::AlreadyExists
            } else {
                if b.row.generation == 1 {
                    t.root_changesets += 1;
                }
                t.changesets.insert(child, b.row);
                changed = true;
                InsertOutcome::Inserted
            };
            for p in &b.parents {
                let mut set = t.children.get(p).cloned().unwrap_or_default();
                if set.insert(child).is_none() {
                    t.children.insert(*p, set);
                    changed = true;
                }
            }
            for row in b.object_paths {
                let mut m = t.object_paths.get(&row.object).cloned().unwrap_or_default();
                if !m.contains_key(&row.path) {
                    let obj = row.object;
                    m.insert(row.path.clone(), row);
                    t.object_paths.insert(obj, m);
                    changed = true;
                }
            }
            for row in b.derived {
                let k = dkey(&row.key);
                if !t.derived.contains_key(&k) {
                    t.derived.insert(k, row);
                    changed = true;
                }
            }
            Ok((outcome, changed))
        })
    }

    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        self.read(Consistency::Strong, |t| Ok(t.refs.get(name).cloned()))
    }

    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        self.read(Consistency::Strong, |t| {
            Ok(t.refs.values().cloned().collect())
        })
    }

    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        self.write(|t| {
            if cas.expect == RefExpect::Absent && cas.new_target.is_none() {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    "cas_ref: cannot delete a ref expected to be absent",
                ));
            }
            let current = t.refs.get(&cas.name).cloned();
            let matches = match (&cas.expect, &current) {
                (RefExpect::Absent, None) => true,
                (RefExpect::Version(v), Some(r)) => r.version == *v,
                _ => false,
            };
            if !matches {
                return Ok((CasOutcome::VersionMismatch { current }, false));
            }
            let mut seen = BTreeSet::new();
            for e in &cas.landed {
                if !seen.insert(e.change_id) {
                    return Err(ScionError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "cas_ref: duplicate landed change id {}",
                            e.change_id.to_hex()
                        ),
                    ));
                }
                if let Some(existing) = t.landed.get(&(cas.name.clone(), e.change_id)) {
                    return Ok((
                        CasOutcome::LandedConflict {
                            existing: existing.clone(),
                        },
                        false,
                    ));
                }
            }
            let log = t.ref_log.get(&cas.name).cloned().unwrap_or_default();
            let new_version = match &cas.expect {
                RefExpect::Version(v) => v.checked_add(1),
                RefExpect::Absent => log.get_max().map_or(Some(1), |(v, _)| v.checked_add(1)),
            }
            .ok_or_else(|| ScionError::new(ErrorCode::InvalidArgument, "ref version overflow"))?;
            let new_row = cas.new_target.map(|target| RefRow {
                name: cas.name.clone(),
                target,
                version: new_version,
                updated_at: cas.at,
            });
            match &new_row {
                Some(r) => {
                    t.refs.insert(cas.name.clone(), r.clone());
                }
                None => {
                    t.refs.remove(&cas.name);
                }
            }
            let mut log = log;
            log.insert(
                new_version,
                RefLogRow {
                    name: cas.name.clone(),
                    version: new_version,
                    old_target: current.map(|r| r.target),
                    new_target: cas.new_target,
                    principal: cas.principal.clone(),
                    at: cas.at,
                    landed: cas.landed.clone(),
                },
            );
            t.ref_log.insert(cas.name.clone(), log);
            for e in &cas.landed {
                t.landed.insert(
                    (cas.name.clone(), e.change_id),
                    LandedRow {
                        ref_name: cas.name.clone(),
                        change_id: e.change_id,
                        version: new_version,
                        source: e.source,
                        result: e.result,
                    },
                );
            }
            if let Some(a) = cas.audit {
                t.audit.push_back(a);
            }
            Ok((CasOutcome::Committed(new_row), true))
        })
    }

    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        self.read(Consistency::Strong, |t| {
            Ok(t.ref_log
                .get(name)
                .map(|m| {
                    m.range(from_version..)
                        .take(limit)
                        .map(|(_, r)| r.clone())
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        self.read(Consistency::Strong, |t| {
            Ok(t.landed.get(&(name.clone(), *change_id)).cloned())
        })
    }

    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        self.read(c, |t| Ok(t.derived.get(&dkey(k)).cloned()))
    }

    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        self.write(|t| {
            let mut changed = false;
            for row in rows {
                let k = dkey(&row.key);
                if !t.derived.contains_key(&k) {
                    t.derived.insert(k, row);
                    changed = true;
                }
            }
            Ok(((), changed))
        })
    }

    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        self.write(|t| {
            let doomed: Vec<DKey> = t
                .derived
                .iter()
                .filter(|(k, r)| r.source_object.as_ref() == Some(source) && types.contains(&k.0))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &doomed {
                t.derived.remove(k);
            }
            Ok((doomed.len() as u64, !doomed.is_empty()))
        })
    }

    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        let expires = now
            .checked_add(ttl)
            .ok_or_else(|| ScionError::new(ErrorCode::InvalidArgument, "lease ttl overflow"))?;
        self.write(|t| {
            let key = dkey(k);
            if let Some((h, exp)) = t.leases.get(&key)
                && h != holder
                && *exp > now
            {
                return Ok((false, false));
            }
            t.leases.insert(key, (holder.to_owned(), expires));
            Ok((true, true))
        })
    }

    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        self.write(|t| {
            let key = dkey(k);
            match t.leases.get(&key) {
                Some((h, _)) if h == holder => {
                    t.leases.remove(&key);
                    Ok(((), true))
                }
                _ => Ok(((), false)),
            }
        })
    }

    async fn redact(&self, tx: RedactTxn) -> Result<u64> {
        self.write(|t| {
            if let Some(missing) = tx.ids.iter().find(|id| !t.objects.contains_key(*id)) {
                return Err(ScionError::not_found(format!(
                    "cannot redact unknown object {missing}"
                )));
            }
            let epoch = t.redaction_epoch + 1;
            t.redaction_epoch = epoch;
            for id in &tx.ids {
                match t.redactions.get(id).cloned() {
                    None => {
                        t.redactions.insert(
                            *id,
                            RedactionRow {
                                object: *id,
                                state: RedactionState::Quarantined,
                                reason: tx.reason.clone(),
                                actor: tx.actor.clone(),
                                created_at: tx.at,
                                purge_after: tx.purge_after,
                                epoch,
                            },
                        );
                    }
                    Some(mut r) if r.state == RedactionState::Quarantined => {
                        r.reason = tx.reason.clone();
                        r.actor = tx.actor.clone();
                        r.purge_after = tx.purge_after;
                        r.epoch = epoch;
                        t.redactions.insert(*id, r);
                    }
                    Some(_) => {}
                }
                if let Some(mut o) = t.objects.get(id).cloned()
                    && o.state != ObjectState::Purged
                {
                    o.state = ObjectState::Quarantined;
                    t.objects.insert(*id, o);
                }
                t.audit.push_back(AuditRow {
                    at: tx.at,
                    actor: tx.actor.clone(),
                    action: "redact".to_owned(),
                    target: id.to_text(),
                    reason: tx.reason.clone(),
                    detail: String::new(),
                });
            }
            Ok((epoch, true))
        })
    }

    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        self.write(|t| {
            for id in ids {
                let quarantined = t
                    .redactions
                    .get(id)
                    .is_some_and(|r| r.state == RedactionState::Quarantined);
                if !quarantined {
                    return Err(invalid(
                        subcode::NOT_QUARANTINED,
                        format!("object {id} is not quarantined"),
                    ));
                }
            }
            let epoch = t.redaction_epoch + 1;
            t.redaction_epoch = epoch;
            for id in ids {
                t.redactions.remove(id);
                if let Some(mut o) = t.objects.get(id).cloned() {
                    o.state = ObjectState::Live;
                    t.objects.insert(*id, o);
                }
            }
            t.audit.push_back(audit);
            Ok((epoch, true))
        })
    }

    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        self.write(|t| {
            for id in ids {
                if let Some(mut r) = t.redactions.get(id).cloned() {
                    r.state = RedactionState::Purged;
                    t.redactions.insert(*id, r);
                }
                if let Some(mut o) = t.objects.get(id).cloned() {
                    o.state = ObjectState::Purged;
                    o.inline_data = None;
                    t.objects.insert(*id, o);
                }
            }
            t.audit.push_back(audit);
            Ok(((), true))
        })
    }

    async fn redaction_epoch(&self) -> Result<u64> {
        self.read(Consistency::Strong, |t| Ok(t.redaction_epoch))
    }

    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        self.read(Consistency::Strong, |t| {
            let mut rows: Vec<RedactionRow> = t
                .redactions
                .values()
                .filter(|r| r.epoch > since_epoch)
                .cloned()
                .collect();
            rows.sort_by_key(|a| (a.epoch, a.object));
            Ok((rows, t.redaction_epoch))
        })
    }

    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        self.read(Consistency::Strong, |t| {
            let mut due: Vec<(SystemTime, ObjectId)> = t
                .redactions
                .values()
                .filter(|r| r.state == RedactionState::Quarantined && r.purge_after <= now)
                .map(|r| (r.purge_after, r.object))
                .collect();
            due.sort();
            Ok(due.into_iter().take(limit).map(|(_, id)| id).collect())
        })
    }

    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        self.write(|t| {
            t.audit.push_back(row);
            Ok(((), true))
        })
    }

    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        self.read(Consistency::Strong, |t| {
            let skip = t.audit.len().saturating_sub(limit);
            Ok(t.audit.iter().skip(skip).cloned().collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_deterministic_and_non_negative() {
        let a = compute_repo_id("test");
        assert_eq!(a, compute_repo_id("test"));
        assert!(a.0 >= 0);
        assert_ne!(a, compute_repo_id("other"));
    }

    #[test]
    fn prefix_helpers() {
        assert!(valid_hex_prefix(""));
        assert!(valid_hex_prefix("0123abcd"));
        assert!(!valid_hex_prefix("ABCD"));
        assert!(!valid_hex_prefix(&"0".repeat(65)));
        let f = prefix_floor("ab").unwrap();
        assert_eq!(f.hex(), format!("ab{}", "0".repeat(62)));
        let f = prefix_floor("abc").unwrap();
        assert_eq!(f.hex(), format!("abc{}", "0".repeat(61)));
    }
}
