//! SPEC 4.5 invariant oracle used by every crash test (`docs/contracts/storage-semantics.md` 8).
//!
//! `fsck` walks every changeset row (`changesets_by_prefix("")`) and every ref and reports:
//!
//! - `MissingObject`: a missing root tree (`root_tree`), meta (`meta`), parent changeset
//!   (`parent`), delta blob (`delta_blob`), subtree met while computing the delta (`tree`), or,
//!   with `verify_bytes`, any other tree entry (`tree_entry`) (SPEC 4.5 invariant 2).
//! - `DigestMismatch`: stored bytes that do not hash to their id (invariant 1). Changeset rows
//!   and every tree read for the delta check are always verified; with `verify_bytes` every live
//!   object reachable from a changeset (all trees, blobs and metas) is verified as well, and so
//!   is every other live object whose bytes are in object storage (found by listing the live
//!   key area; bytes without a row are orphans, not violations).
//! - `BadEncoding`: bytes that do not decode, or do not re-encode to the same bytes.
//! - `DeltaMismatch`: `delta != FileDiff(T1, root_tree)` (SPEC 4.3.4).
//! - `MissingEager`: a required eager derived value is absent. Per-changeset types are looked up
//!   under `DerivedKey { ty, version, key: <34 raw changeset id bytes> }`; `commit_graph` lives on
//!   the changesets row (checked as `BadGraph`), `path_index` is checked as an `object_paths` row
//!   for every `Put` of the delta and every new tree (a tree of the root tree that differs from
//!   `T1` at its directory path, including the root tree at the empty path; SPEC 9.2), and
//!   `git_mapping` (import only) is not required.
//! - `BadGraph`: `generation != 1 + max(parent generations)` or wrong skip pointers
//!   (`skip[k]` = first-parent ancestor at distance `2^k`, SPEC 9.2).
//! - `RefTargetMissing`: a ref whose target changeset row is absent.
//!
//! Quarantined or purged objects are never violations (their rows exist; their bytes are
//! intentionally not served).

use crate::repo::ObjectRepo;
use crate::types::{ChangesetRow, Consistency, DerivedKey};
use bytes::Bytes;
use lru::LruCache;
use scion_core::{ErrorCode, Kind, Mode, ObjectId, Result, ScionError, empty_tree_id};
use scion_object::{Canonical, Changeset, DeltaOp, Meta, Tree, TreeEntry};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

/// fsck options.
#[derive(Clone, Debug)]
pub struct FsckOptions {
    /// Eager derived types `(name, version)` every changeset must have.
    pub required_eager: Vec<(String, u32)>,
    /// Also verify the digest of every live object's bytes.
    pub verify_bytes: bool,
}

/// One SPEC 4.5 invariant violation.
#[derive(Clone, Debug, PartialEq)]
pub enum Violation {
    /// A changeset references an object that does not exist.
    MissingObject {
        /// The changeset.
        changeset: ObjectId,
        /// The missing object.
        object: ObjectId,
        /// `root_tree`, `meta`, `parent`, `delta_blob`, `tree` or `tree_entry`.
        role: &'static str,
    },
    /// Stored bytes do not hash to the id.
    DigestMismatch {
        /// The object.
        id: ObjectId,
    },
    /// Stored bytes do not decode canonically.
    BadEncoding {
        /// The object.
        id: ObjectId,
    },
    /// `delta != FileDiff(parent tree, root tree)`.
    DeltaMismatch {
        /// The changeset.
        changeset: ObjectId,
    },
    /// A required eager derived row is missing.
    MissingEager {
        /// The changeset.
        changeset: ObjectId,
        /// The type name.
        ty: String,
    },
    /// Generation or skip pointers are inconsistent.
    BadGraph {
        /// The changeset.
        changeset: ObjectId,
    },
    /// A ref points at a missing changeset.
    RefTargetMissing {
        /// The ref name.
        name: String,
    },
}

/// Eager types stored somewhere other than the `derived` table, or not required everywhere.
const COMMIT_GRAPH: &str = "commit_graph";
const PATH_INDEX: &str = "path_index";
const GIT_MAPPING: &str = "git_mapping";

/// Decoded trees fsck keeps while walking deltas (consecutive changesets share most trees).
const TREE_CACHE_ENTRIES: NonZeroUsize = match NonZeroUsize::new(4096) {
    Some(n) => n,
    None => NonZeroUsize::MIN,
};

/// "No limit" for list reads; SQL backends bind limits as INT8, so stay within `i64`.
const UNBOUNDED: usize = i64::MAX as usize;

/// A file-level change: path bytes and the resulting `(mode, id, size)` (`None` = delete).
type FileChange = (Vec<u8>, Option<(Mode, ObjectId, u64)>);

/// The result of the delta walk.
struct Diff {
    /// `FileDiff(T1, root_tree)`, ascending by path.
    changes: Vec<FileChange>,
    /// Trees of `root_tree` that differ from `T1` at their directory path, with that path.
    new_trees: Vec<(Vec<u8>, ObjectId)>,
}

struct Checker<'a> {
    objects: &'a ObjectRepo,
    opts: &'a FsckOptions,
    out: Vec<Violation>,
    rows: HashMap<ObjectId, Option<ChangesetRow>>,
    decoded: HashMap<ObjectId, Option<Arc<Changeset>>>,
    verified: HashSet<ObjectId>,
    trees: LruCache<ObjectId, Arc<Tree>>,
}

fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(prefix.len() + 1 + name.len());
    if !prefix.is_empty() {
        p.extend_from_slice(prefix);
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

fn canonical<T: Canonical>(raw: &[u8]) -> Option<T> {
    T::decode(raw).ok().filter(|v| v.encode() == raw)
}

impl<'a> Checker<'a> {
    fn push(&mut self, v: Violation) {
        if !self.out.contains(&v) {
            self.out.push(v);
        }
    }

    async fn row(&mut self, id: &ObjectId) -> Result<Option<ChangesetRow>> {
        if let Some(r) = self.rows.get(id) {
            return Ok(r.clone());
        }
        let r = self
            .objects
            .storage()
            .meta
            .get_changesets(&[*id], Consistency::Strong)
            .await?
            .pop()
            .flatten();
        self.rows.insert(*id, r.clone());
        Ok(r)
    }

    /// The decoded changeset, or `None` when its row is missing, corrupt or non-canonical.
    async fn decoded(&mut self, id: &ObjectId) -> Result<Option<Arc<Changeset>>> {
        if let Some(d) = self.decoded.get(id) {
            return Ok(d.clone());
        }
        let d = self.row(id).await?.and_then(|r| {
            let hash_ok = ObjectId::hash_encoded(Kind::Changeset, &r.encoded) == *id;
            hash_ok
                .then(|| canonical::<Changeset>(&r.encoded))
                .flatten()
                .map(Arc::new)
        });
        self.decoded.insert(*id, d.clone());
        Ok(d)
    }

    async fn exists(&self, id: &ObjectId) -> Result<bool> {
        Ok(self.objects.stat(id).await?.is_some())
    }

    /// A tree for the delta walk; violations are recorded and `None` returned when it cannot
    /// be read. Trees are read from storage (verified), never from the repo's decoded LRU, so
    /// a tree cached before its row or bytes went bad is still reported; fsck keeps its own
    /// bounded cache.
    async fn tree(&mut self, cs: &ObjectId, id: &ObjectId) -> Result<Option<Arc<Tree>>> {
        if *id == empty_tree_id() {
            return Ok(Some(Arc::new(Tree::default())));
        }
        if let Some(t) = self.trees.get(id) {
            return Ok(Some(t.clone()));
        }
        if id.kind() != Kind::Tree {
            self.push(Violation::BadEncoding { id: *id });
            return Ok(None);
        }
        match self.objects.get(id).await {
            Ok(raw) => match canonical::<Tree>(&raw) {
                Some(t) => {
                    let t = Arc::new(t);
                    self.trees.put(*id, t.clone());
                    Ok(Some(t))
                }
                None => {
                    self.push(Violation::BadEncoding { id: *id });
                    Ok(None)
                }
            },
            Err(e) => self.object_error(cs, id, "tree", e).map(|()| None),
        }
    }

    /// Map an object read error to a violation; infrastructure errors propagate.
    fn object_error(
        &mut self,
        cs: &ObjectId,
        id: &ObjectId,
        role: &'static str,
        e: ScionError,
    ) -> Result<()> {
        match e.code {
            ErrorCode::NotFound => self.push(Violation::MissingObject {
                changeset: *cs,
                object: *id,
                role,
            }),
            ErrorCode::Integrity => self.push(Violation::DigestMismatch { id: *id }),
            ErrorCode::InvalidArgument => self.push(Violation::BadEncoding { id: *id }),
            ErrorCode::Redacted => {}
            _ => return Err(e),
        }
        Ok(())
    }

    /// FileDiff(a, b) over trees, skipping equal subtrees, plus every tree of `b` that differs
    /// from `a` at its directory path (the "new trees" of SPEC 9.2 `path_index`). `None` when a
    /// tree is unreadable. Entries are merge-joined (both lists are sorted by name, SPEC 4.3.2),
    /// so the walk costs O(entries of the differing trees).
    async fn file_diff(&mut self, cs: &ObjectId, a: ObjectId, b: ObjectId) -> Result<Option<Diff>> {
        let mut changes: Vec<FileChange> = Vec::new();
        let mut new_trees: Vec<(Vec<u8>, ObjectId)> = Vec::new();
        let mut stack: Vec<(Vec<u8>, Option<ObjectId>, Option<ObjectId>)> =
            vec![(Vec::new(), Some(a), Some(b))];
        while let Some((prefix, ta, tb)) = stack.pop() {
            if ta == tb {
                continue;
            }
            if let Some(tb) = tb {
                new_trees.push((prefix.clone(), tb));
            }
            let mut sides: [Option<Arc<Tree>>; 2] = [None, None];
            for (slot, id) in sides.iter_mut().zip([ta, tb]) {
                if let Some(id) = id {
                    match self.tree(cs, &id).await? {
                        Some(t) => *slot = Some(t),
                        None => return Ok(None),
                    }
                }
            }
            let empty = Tree::default();
            let ea = &sides[0].as_deref().unwrap_or(&empty).entries;
            let eb = &sides[1].as_deref().unwrap_or(&empty).entries;
            let (mut i, mut j) = (0usize, 0usize);
            while i < ea.len() || j < eb.len() {
                let (fa, fb) = match (ea.get(i), eb.get(j)) {
                    (Some(x), Some(y)) => match x.name.as_bytes().cmp(y.name.as_bytes()) {
                        Ordering::Less => (Some(x), None),
                        Ordering::Greater => (None, Some(y)),
                        Ordering::Equal => (Some(x), Some(y)),
                    },
                    (x, y) => (x, y),
                };
                // At least one side is present: the loop condition guarantees it.
                let Some(name) = fa.or(fb).map(|e| e.name.as_bytes()) else {
                    break;
                };
                if fa.is_some() {
                    i += 1;
                }
                if fb.is_some() {
                    j += 1;
                }
                let file = |e: Option<&TreeEntry>| {
                    e.filter(|e| e.mode != Mode::Tree)
                        .map(|e| (e.mode, e.id, e.size))
                };
                let dir = |e: Option<&TreeEntry>| e.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                let path = join(&prefix, name);
                let (fa, fb, da, db) = (file(fa), file(fb), dir(fa), dir(fb));
                let same_file = match (fa, fb) {
                    (Some(x), Some(y)) => x.0 == y.0 && x.1 == y.1,
                    (None, None) => true,
                    _ => false,
                };
                if !same_file {
                    changes.push((path.clone(), fb));
                }
                if da != db {
                    stack.push((path, da, db));
                }
            }
        }
        changes.sort_by(|x, y| x.0.cmp(&y.0));
        new_trees.sort_by(|x, y| x.0.cmp(&y.0));
        Ok(Some(Diff { changes, new_trees }))
    }

    /// Verify the bytes of every live object reachable from `root` plus `meta`.
    async fn verify_reachable(
        &mut self,
        cs: &ObjectId,
        root: ObjectId,
        meta: ObjectId,
    ) -> Result<()> {
        if self.verified.insert(meta) {
            match self.objects.get(&meta).await {
                Ok(raw) => {
                    if canonical::<Meta>(&raw).is_none() {
                        self.push(Violation::BadEncoding { id: meta });
                    }
                }
                Err(e) => self.object_error(cs, &meta, "meta", e)?,
            }
        }
        let mut stack = vec![root];
        while let Some(t) = stack.pop() {
            if !self.verified.insert(t) {
                continue;
            }
            let raw = match self.objects.get(&t).await {
                Ok(raw) => raw,
                Err(e) => {
                    self.object_error(cs, &t, "tree", e)?;
                    continue;
                }
            };
            let Some(tree) = canonical::<Tree>(&raw) else {
                self.push(Violation::BadEncoding { id: t });
                continue;
            };
            for e in &tree.entries {
                if e.mode == Mode::Tree {
                    stack.push(e.id);
                } else if self.verified.insert(e.id) {
                    match self.objects.get(&e.id).await {
                        Ok(_) => {}
                        Err(err) => self.object_error(cs, &e.id, "tree_entry", err)?,
                    }
                }
            }
        }
        Ok(())
    }

    /// True when `object_paths` has the row `(id, path)`.
    async fn has_path_row(&self, id: &ObjectId, path: &[u8]) -> Result<bool> {
        let rows = self
            .objects
            .storage()
            .meta
            .object_paths(id, UNBOUNDED)
            .await?;
        Ok(rows.iter().any(|r| r.path.as_bytes() == path))
    }

    /// `path_index` (SPEC 9.2): a row for every `Put` blob at its path and, when the delta walk
    /// succeeded, every new tree at its directory path.
    async fn path_index_present(
        &self,
        cs: &Changeset,
        new_trees: Option<&[(Vec<u8>, ObjectId)]>,
    ) -> Result<bool> {
        for d in &cs.delta {
            if let DeltaOp::Put { id: blob, .. } = &d.op
                && !self.has_path_row(blob, d.path.as_bytes()).await?
            {
                return Ok(false);
            }
        }
        for (path, tree) in new_trees.unwrap_or_default() {
            if !self.has_path_row(tree, path).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn check_eager(
        &mut self,
        id: &ObjectId,
        cs: &Changeset,
        new_trees: Option<&[(Vec<u8>, ObjectId)]>,
    ) -> Result<()> {
        let meta = self.objects.storage().meta.clone();
        for (ty, version) in &self.opts.required_eager {
            let present = match ty.as_str() {
                COMMIT_GRAPH | GIT_MAPPING => true,
                PATH_INDEX => self.path_index_present(cs, new_trees).await?,
                _ => {
                    let k = DerivedKey {
                        ty: ty.clone(),
                        version: *version,
                        key: Bytes::copy_from_slice(id.as_bytes()),
                    };
                    meta.get_derived(&k, Consistency::Strong).await?.is_some()
                }
            };
            if !present {
                self.push(Violation::MissingEager {
                    changeset: *id,
                    ty: ty.clone(),
                });
            }
        }
        Ok(())
    }

    async fn check_graph(
        &mut self,
        id: &ObjectId,
        row: &ChangesetRow,
        cs: &Changeset,
    ) -> Result<()> {
        let mut parent_rows = Vec::with_capacity(cs.parents.len());
        for p in &cs.parents {
            match self.row(p).await? {
                Some(r) => parent_rows.push(r),
                None => return Ok(()), // reported as MissingObject(parent)
            }
        }
        let generation = parent_rows
            .iter()
            .map(|r| r.generation)
            .max()
            .map_or(1, |g| g.saturating_add(1));
        let mut skip: Vec<ObjectId> = Vec::new();
        if let Some(first) = cs.parents.first() {
            skip.push(*first);
            loop {
                let k = skip.len();
                let Some(prev) = skip.last().copied() else {
                    break;
                };
                let Some(prev_row) = self.row(&prev).await? else {
                    break;
                };
                match prev_row.skip.get(k - 1) {
                    Some(next) => skip.push(*next),
                    None => break,
                }
            }
        }
        if row.generation != generation || row.skip != skip {
            self.push(Violation::BadGraph { changeset: *id });
        }
        Ok(())
    }

    async fn changeset(&mut self, id: &ObjectId) -> Result<()> {
        let Some(row) = self.row(id).await? else {
            return Ok(());
        };
        if ObjectId::hash_encoded(Kind::Changeset, &row.encoded) != *id {
            self.push(Violation::DigestMismatch { id: *id });
            return Ok(());
        }
        let Some(cs) = self.decoded(id).await? else {
            self.push(Violation::BadEncoding { id: *id });
            return Ok(());
        };

        let root_ok = self.exists(&cs.root_tree).await?;
        if !root_ok {
            self.push(Violation::MissingObject {
                changeset: *id,
                object: cs.root_tree,
                role: "root_tree",
            });
        }
        let meta_ok = self.exists(&cs.meta).await?;
        if !meta_ok {
            self.push(Violation::MissingObject {
                changeset: *id,
                object: cs.meta,
                role: "meta",
            });
        }
        let mut parents_ok = true;
        for p in &cs.parents {
            if self.row(p).await?.is_none() {
                parents_ok = false;
                self.push(Violation::MissingObject {
                    changeset: *id,
                    object: *p,
                    role: "parent",
                });
            }
        }
        let blobs: Vec<ObjectId> = cs
            .delta
            .iter()
            .filter_map(|d| match &d.op {
                DeltaOp::Put { id, .. } => Some(*id),
                DeltaOp::Delete => None,
            })
            .collect();
        let stats = self.objects.stat_many(&blobs).await?;
        for (b, s) in blobs.iter().zip(stats) {
            if s.is_none() {
                self.push(Violation::MissingObject {
                    changeset: *id,
                    object: *b,
                    role: "delta_blob",
                });
            }
        }

        self.check_graph(id, &row, &cs).await?;

        // delta == FileDiff(T1, root_tree), T1 = root tree of parents[0] or the empty tree.
        let mut new_trees: Option<Vec<(Vec<u8>, ObjectId)>> = None;
        if root_ok && parents_ok {
            let t1 = match cs.parents.first() {
                None => Some(empty_tree_id()),
                Some(p) => match self.decoded(p).await? {
                    Some(pc) if self.exists(&pc.root_tree).await? => Some(pc.root_tree),
                    _ => None,
                },
            };
            if let Some(t1) = t1
                && let Some(diff) = self.file_diff(id, t1, cs.root_tree).await?
            {
                let expected: Vec<FileChange> = cs
                    .delta
                    .iter()
                    .map(|d| {
                        let op = match &d.op {
                            DeltaOp::Put { mode, id, size } => Some((*mode, *id, *size)),
                            DeltaOp::Delete => None,
                        };
                        (d.path.as_bytes().to_vec(), op)
                    })
                    .collect();
                if diff.changes != expected {
                    self.push(Violation::DeltaMismatch { changeset: *id });
                }
                new_trees = Some(diff.new_trees);
            }
        }

        self.check_eager(id, &cs, new_trees.as_deref()).await?;

        if self.opts.verify_bytes && root_ok && meta_ok {
            self.verify_reachable(id, cs.root_tree, cs.meta).await?;
        }
        Ok(())
    }
}

/// The object id stored under a live key `<prefix>/r/<repo_id>/<letter>/<hex[0:2]>/<hex>`
/// (SPEC 5.1), given the key with `<prefix>/r/<repo_id>/` stripped. `None` for foreign keys.
fn id_from_live_key(rest: &str) -> Option<ObjectId> {
    let mut parts = rest.split('/');
    let (letter, fan, hex) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || hex.len() != 64 || !hex.starts_with(fan) {
        return None;
    }
    let kind = match letter {
        "b" => Kind::Blob,
        "t" => Kind::Tree,
        "c" => Kind::Changeset,
        "m" => Kind::Meta,
        "x" => Kind::Derived(0x10),
        x => {
            let k = u8::from_str_radix(x.strip_prefix('x')?, 16).ok()?;
            match Kind::from_byte(k).ok()? {
                d @ Kind::Derived(n) if n != 0x10 => d,
                _ => return None,
            }
        }
    };
    let mut digest = [0u8; 32];
    hex::decode_to_slice(hex, &mut digest).ok()?;
    let id = ObjectId::new(kind, digest);
    (id.hex() == hex).then_some(id)
}

impl Checker<'_> {
    /// SPEC 4.5 invariant 1 for objects no changeset reaches: every live object with bytes in
    /// object storage must hash to its id. Bytes without a row are harmless orphans (SPEC 5.3)
    /// and redacted objects are skipped (SPEC 13.4).
    async fn verify_live_area(&mut self) -> Result<()> {
        let objects = self.objects;
        let prefix = format!(
            "{}/r/{}/",
            objects.config().prefix,
            objects.storage().meta.repo_id().0
        );
        let keys = objects.storage().blobs.list(&prefix).await?;
        for key in keys {
            let Some(id) = key.strip_prefix(&prefix).and_then(id_from_live_key) else {
                continue;
            };
            if objects.key(&id, crate::types::Area::Live) != key || !self.verified.insert(id) {
                continue;
            }
            match objects.get(&id).await {
                Ok(_) => {}
                Err(e) => match e.code {
                    ErrorCode::NotFound | ErrorCode::Redacted => {}
                    ErrorCode::Integrity => self.push(Violation::DigestMismatch { id }),
                    _ => return Err(e),
                },
            }
        }
        Ok(())
    }
}

/// Walk every changeset and ref and report SPEC 4.5 violations. Quarantined or purged bytes
/// are not violations. Infrastructure failures (UNAVAILABLE) are returned as errors.
pub async fn fsck(objects: &ObjectRepo, opts: &FsckOptions) -> Result<Vec<Violation>> {
    let mut ck = Checker {
        objects,
        opts,
        out: Vec::new(),
        rows: HashMap::new(),
        decoded: HashMap::new(),
        verified: HashSet::new(),
        trees: LruCache::new(TREE_CACHE_ENTRIES),
    };
    let meta = objects.storage().meta.clone();
    let ids = meta.changesets_by_prefix("", UNBOUNDED).await?;
    for id in &ids {
        ck.changeset(id).await?;
    }
    for r in meta.list_refs().await? {
        if ck.row(&r.target).await?.is_none() {
            ck.push(Violation::RefTargetMissing {
                name: r.name.as_str().to_owned(),
            });
        }
    }
    if opts.verify_bytes {
        ck.verify_live_area().await?;
    }
    Ok(ck.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::object_key;
    use crate::types::{Area, RepoId};

    #[test]
    fn live_keys_parse_back_to_ids() {
        for kind in [
            Kind::Blob,
            Kind::Tree,
            Kind::Meta,
            Kind::Changeset,
            Kind::Derived(0x10),
            Kind::Derived(0x2a),
        ] {
            let id = ObjectId::new(kind, [0x5c; 32]);
            let key = object_key("p", RepoId(9), &id, Area::Live);
            let rest = key.strip_prefix("p/r/9/").unwrap();
            assert_eq!(id_from_live_key(rest), Some(id), "{key}");
        }
        let hex = "ab".repeat(32);
        for bad in [
            format!("b/cd/{hex}"),
            format!("b/ab/{hex}/extra"),
            format!("q/ab/{hex}"),
            format!("x10/ab/{hex}"),
            format!("x05/ab/{hex}"),
            format!("b/ab/{}", "AB".repeat(32)),
            "b/ab/abcd".to_owned(),
        ] {
            assert_eq!(id_from_live_key(&bad), None, "{bad}");
        }
    }
}
