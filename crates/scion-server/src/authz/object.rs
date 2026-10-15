//! SPEC 18.6 `can_read_object` (P25): retroactive object authorization through `object_paths`.
//!
//! An object is readable by a principal when one of its `object_paths` rows has a path the
//! current policy lets that principal read (SPEC 12.5). Only the rows of the requested id are
//! read, so the cost is independent of repository size (SPEC 19.18), and a policy change takes
//! effect immediately with no reindex (the rows are evaluated against the policy current at
//! call time).
//!
//! The access class follows the readable rows: `Root` when some row the principal can read is
//! governed by the `/` read rule, `Restricted` otherwise. A positive decision with class `Root`
//! therefore implies the principal reads `/`, which is what lets a daemon confirm root readers
//! from it (PLAN 6.13).
//!
//! Decisions are cached by `(principal, id, policy_epoch)` (SPEC 12.5, 18.6) in a bounded LRU.
//! Only `Allow` decisions are cached: `object_paths` rows are only ever added, so an `Allow`
//! stays true for a given policy, while a `Deny` can become an `Allow` as soon as a changeset
//! references the object at a readable path (for example right after the caller uploaded it
//! and created the changeset). Redaction is checked before the cache, so a cached `Allow` never
//! outlives a redaction.

use super::{AccessClass, ObjectAuthorizer, ReadDecision};
use lru::LruCache;
use parking_lot::Mutex;
use scion_core::{Kind, ObjectId, Principal, Result};
use scion_object::RepoPath;
use scion_policy::Policy;
use scion_store::ObjectPathRow;
use std::num::NonZeroUsize;

/// Capacity of the decision cache (entries).
pub const DECISION_CACHE_ENTRIES: usize = 65_536;

/// The most `object_paths` rows read for one object. Every row of the object is read in one
/// metadata call (SPEC 19.18 counts exactly the object's rows); an object referenced at more
/// paths than this is evaluated over its first rows in path order.
pub const MAX_OBJECT_PATH_ROWS: usize = 100_000;

type CacheKey = (Principal, ObjectId, u64);

/// Private per-authorizer state owned by P25: the `(principal, id, policy_epoch)` decision cache
/// of SPEC 18.6 / PLAN 2.14. `ObjectAuthorizer::new` builds it with `Default`.
pub(crate) struct AuthzState {
    cache: Mutex<LruCache<CacheKey, AccessClass>>,
}

impl std::fmt::Debug for AuthzState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthzState")
            .field("cached", &self.cache.lock().len())
            .finish()
    }
}

impl Default for AuthzState {
    fn default() -> Self {
        let cap = NonZeroUsize::new(DECISION_CACHE_ENTRIES).unwrap_or(NonZeroUsize::MIN);
        AuthzState {
            cache: Mutex::new(LruCache::new(cap)),
        }
    }
}

/// A SPEC 18.6 decision together with whether the object is referenced at all (has
/// `object_paths` rows, or is a Meta that exists). Handlers use `referenced` to tell `denied`
/// (referenced but not readable) from `not_found` (absent or unreferenced) without revealing
/// unreferenced uploads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// The SPEC 18.6 decision.
    pub decision: ReadDecision,
    /// True when the object is referenced by history (rows exist), or for a cached `Allow`.
    pub referenced: bool,
}

/// SPEC 12.5 access class of `path` for a principal who can read it.
fn class_of(policy: &Policy, path: &RepoPath) -> AccessClass {
    if policy.read_governed_by_root(path) {
        AccessClass::Root
    } else {
        AccessClass::Restricted
    }
}

/// SPEC 12.5 decision over the `object_paths` rows of one object: `Allow` with the best
/// class among readable rows, `Deny` otherwise; plus whether any row exists.
fn decide(policy: &Policy, p: &Principal, rows: &[ObjectPathRow]) -> (ReadDecision, bool) {
    let mut class = None;
    for row in rows {
        if !policy.can_read(p, &row.path) {
            continue;
        }
        let c = class_of(policy, &row.path);
        class = Some(c);
        if c == AccessClass::Root {
            break;
        }
    }
    let decision = match class {
        Some(c) => ReadDecision::Allow(c),
        None => ReadDecision::Deny,
    };
    (decision, !rows.is_empty())
}

impl ObjectAuthorizer {
    /// SPEC 18.6 / 12.5: redacted first (`Redacted`), then the `(principal, id, policy_epoch)`
    /// decision cache, then the `object_paths` rows of this id only, evaluated against the
    /// current policy: `Allow(Root)` when a readable row is governed by the `/` rule,
    /// `Allow(Restricted)` when only more specific rules grant it, `Deny` when there are no
    /// rows (unreferenced uploads are readable by no one) or none is readable.
    ///
    /// Meta objects have no `object_paths` rows; they are visible to every repository reader
    /// (SPEC 12.6), so a Meta that exists is `Allow(Root)` for principals who can read `/`.
    pub async fn can_read(&self, p: &Principal, id: &ObjectId) -> Result<ReadDecision> {
        Ok(self.evaluate(p, id).await?.decision)
    }

    /// [`ObjectAuthorizer::can_read`] plus whether the object is referenced (SPEC 7.3: the
    /// `denied` / `not_found` distinction of GetObjects).
    pub async fn evaluate(&self, p: &Principal, id: &ObjectId) -> Result<Evaluation> {
        if self.redactions.contains(id) {
            return Ok(Evaluation {
                decision: ReadDecision::Redacted,
                referenced: true,
            });
        }
        // One policy snapshot for both the cache key and the evaluation.
        let policy = self.policy.current();
        let key: CacheKey = (p.clone(), *id, policy.epoch());
        if let Some(class) = self.state.cache.lock().get(&key).copied() {
            return Ok(Evaluation {
                decision: ReadDecision::Allow(class),
                referenced: true,
            });
        }
        let (decision, referenced) = if id.kind() == Kind::Meta {
            self.evaluate_meta(&policy, p, id).await?
        } else {
            let rows = self
                .storage
                .meta
                .object_paths(id, MAX_OBJECT_PATH_ROWS)
                .await?;
            decide(&policy, p, &rows)
        };
        if let ReadDecision::Allow(class) = decision {
            self.state.cache.lock().put(key, class);
        }
        Ok(Evaluation {
            decision,
            referenced,
        })
    }

    /// [`ObjectAuthorizer::evaluate`] for every id, in input order, against one policy
    /// snapshot. Redaction and the decision cache are checked per id as in `evaluate`; the
    /// `object_paths` rows of every remaining non-Meta id are read in ONE metadata call.
    pub async fn evaluate_many(&self, p: &Principal, ids: &[ObjectId]) -> Result<Vec<Evaluation>> {
        let policy = self.policy.current();
        let epoch = policy.epoch();
        let mut out = Vec::with_capacity(ids.len());
        let mut lookup = Vec::new();
        for id in ids {
            let cached = if self.redactions.contains(id) {
                Some(ReadDecision::Redacted)
            } else {
                let key: CacheKey = (p.clone(), *id, epoch);
                self.state
                    .cache
                    .lock()
                    .get(&key)
                    .copied()
                    .map(ReadDecision::Allow)
            };
            let ev = match cached {
                Some(decision) => Evaluation {
                    decision,
                    referenced: true,
                },
                None if id.kind() == Kind::Meta => self.evaluate(p, id).await?,
                None => {
                    lookup.push((out.len(), *id));
                    Evaluation {
                        decision: ReadDecision::Deny,
                        referenced: false,
                    }
                }
            };
            out.push(ev);
        }
        if lookup.is_empty() {
            return Ok(out);
        }
        let lids: Vec<ObjectId> = lookup.iter().map(|(_, id)| *id).collect();
        let rows = self
            .storage
            .meta
            .object_paths_many(&lids, MAX_OBJECT_PATH_ROWS)
            .await?;
        let mut cache = self.state.cache.lock();
        for ((i, id), rows) in lookup.into_iter().zip(rows) {
            let (decision, referenced) = decide(&policy, p, &rows);
            if let ReadDecision::Allow(class) = decision {
                cache.put((p.clone(), id, epoch), class);
            }
            out[i] = Evaluation {
                decision,
                referenced,
            };
        }
        Ok(out)
    }

    /// SPEC 12.6: a Meta is visible to every repository reader (read on `/`). Metas are built
    /// only by the server (UploadObjects rejects them), so an existing Meta is never an
    /// unreferenced client upload (SPEC 12.5).
    async fn evaluate_meta(
        &self,
        policy: &Policy,
        p: &Principal,
        id: &ObjectId,
    ) -> Result<(ReadDecision, bool)> {
        let exists = self
            .storage
            .meta
            .get_objects(std::slice::from_ref(id), scion_store::Consistency::Stale)
            .await?
            .pop()
            .flatten()
            .is_some();
        let exists = exists
            || self
                .storage
                .meta
                .get_objects(std::slice::from_ref(id), scion_store::Consistency::Strong)
                .await?
                .pop()
                .flatten()
                .is_some();
        let decision = if exists && policy.can_read(p, &RepoPath::root()) {
            ReadDecision::Allow(AccessClass::Root)
        } else {
            ReadDecision::Deny
        };
        Ok((decision, exists))
    }

    /// Number of cached decisions (tests and diagnostics).
    pub fn cached_decisions(&self) -> usize {
        self.state.cache.lock().len()
    }

    /// Drop every cached decision (SPEC 12.5; tests).
    pub fn clear_cache(&self) {
        self.state.cache.lock().clear();
    }
}
