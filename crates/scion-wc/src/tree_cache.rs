//! Local tree cache under `.scion/cache` (scion-cache DiskCache) and the status `TreeLookup`
//! backed by it (SPEC 10.1, 11.5; P18).
//!
//! Trees are cached exactly as verified (raw canonical bytes) in a [`DiskCache`] so `status`,
//! `goto` and `sparse` can walk the base tree without contacting the server (SPEC 11.5: status
//! MUST NOT contact the server when every needed tree is cached). Decoded trees are memoized
//! for the lifetime of the handle. The changeset -> root tree mapping is kept in
//! `.scion/cache/roots` so the base root tree is known without a `GetChangesets` call.

use crate::fs_safe::write_atomic;
use crate::status::{Prefixes, TreeLookup};
use bytes::Bytes;
use scion_cache::{DiskCache, Payload};
use scion_core::{EntryInfo, ErrorCode, Kind, Mode, ObjectId, Result, ScionError, empty_tree_id};
use scion_object::{Canonical, RepoPath, Tree};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Directory of the local cache below `.scion/` (SPEC 11.1).
pub(crate) const CACHE_DIR: &str = "cache";
/// File of the changeset -> root tree mapping inside the cache directory.
const ROOTS_FILE: &str = "roots";
/// Disk budget of the local tree cache.
const CACHE_MAX_BYTES: u64 = 8 << 30;
/// Eviction low watermark (fraction of the budget).
const CACHE_LOW_WATERMARK: f64 = 0.9;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Verified, content-addressed local cache of trees (SPEC 10.1 step 4, 11.5).
pub(crate) struct TreeCache {
    disk: DiskCache,
    dir: PathBuf,
    mem: Mutex<HashMap<ObjectId, Arc<Tree>>>,
    roots: Mutex<BTreeMap<ObjectId, ObjectId>>,
}

impl TreeCache {
    /// Open (creating) the cache at `<scion_dir>/cache`.
    pub(crate) fn open(scion_dir: &Path) -> Result<Self> {
        let dir = scion_dir.join(CACHE_DIR);
        std::fs::create_dir_all(&dir)
            .map_err(|e| crate::fs_safe::io_error("create .scion/cache", e))?;
        let disk = DiskCache::open(&dir, CACHE_MAX_BYTES, CACHE_LOW_WATERMARK, true)?;
        let roots = load_roots(&dir.join(ROOTS_FILE));
        Ok(TreeCache {
            disk,
            dir,
            mem: Mutex::new(HashMap::new()),
            roots: Mutex::new(roots),
        })
    }

    /// Whether `id` is available locally (the empty tree always is).
    pub(crate) fn contains(&self, id: &ObjectId) -> bool {
        *id == empty_tree_id() || lock(&self.mem).contains_key(id) || self.disk.contains(id)
    }

    /// The decoded tree, when cached. Entries that fail verification are dropped by the disk
    /// cache and reported as absent.
    pub(crate) fn get(&self, id: &ObjectId) -> Result<Option<Arc<Tree>>> {
        if *id == empty_tree_id() {
            return Ok(Some(Arc::new(Tree::empty())));
        }
        if let Some(t) = lock(&self.mem).get(id) {
            return Ok(Some(t.clone()));
        }
        let Some(raw) = self.disk.get(id)? else {
            return Ok(None);
        };
        let tree = match Tree::decode(&raw) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                tracing::warn!(object = %id, error = %e, "discarding an undecodable cached tree");
                self.disk.remove(id)?;
                return Ok(None);
            }
        };
        lock(&self.mem).insert(*id, tree.clone());
        Ok(Some(tree))
    }

    /// The decoded tree; NOT_FOUND when it is not cached.
    pub(crate) fn require(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        self.get(id)?
            .ok_or_else(|| ScionError::not_found(format!("tree {id} is not in the local cache")))
    }

    /// Verify (BLAKE3), decode (SPEC 4 validation, which refuses malicious names) and cache a
    /// tree fetched from upstream (SPEC 10.1 step 4).
    pub(crate) fn insert_verified(&self, id: &ObjectId, raw: Bytes) -> Result<Arc<Tree>> {
        if id.kind() != Kind::Tree || ObjectId::hash_encoded(Kind::Tree, &raw) != *id {
            return Err(ScionError::integrity(id));
        }
        let tree = Arc::new(Tree::decode(&raw)?);
        self.disk.insert(id, Payload::Raw(raw))?;
        lock(&self.mem).insert(*id, tree.clone());
        Ok(tree)
    }

    /// The cached root tree of changeset `cs`.
    pub(crate) fn root_of(&self, cs: &ObjectId) -> Option<ObjectId> {
        lock(&self.roots).get(cs).copied()
    }

    /// Record the root tree of changeset `cs` (persisted in `.scion/cache/roots`).
    pub(crate) fn set_root(&self, cs: &ObjectId, tree: &ObjectId) -> Result<()> {
        let text = {
            let mut roots = lock(&self.roots);
            if roots.get(cs) == Some(tree) {
                return Ok(());
            }
            roots.insert(*cs, *tree);
            let mut text = String::new();
            for (c, t) in roots.iter() {
                text.push_str(&format!("{c} {t}\n"));
            }
            text
        };
        write_atomic(&self.dir, ROOTS_FILE, text.as_bytes(), 0o644, false)
    }

    /// Walk the cached tree `root` within `scope`, skipping `inaccessible` prefixes; every file
    /// entry in scope (SPEC 11.2). NOT_FOUND when a needed tree is not cached.
    pub(crate) fn files_in_scope(
        &self,
        root: &ObjectId,
        scope: &[RepoPath],
        inaccessible: &[RepoPath],
    ) -> Result<BTreeMap<RepoPath, EntryInfo>> {
        let prefixes = Prefixes::new(scope, inaccessible);
        let mut out = BTreeMap::new();
        let mut stack: Vec<(RepoPath, ObjectId)> = vec![(RepoPath::root(), *root)];
        while let Some((path, id)) = stack.pop() {
            let tree = self.require(&id)?;
            for e in &tree.entries {
                let child = path.join(&e.name)?;
                let b = child.as_bytes();
                if prefixes.inaccessible(b) {
                    continue;
                }
                if e.mode == Mode::Tree {
                    if prefixes.in_scope(b) || prefixes.leads_to_scope(b) {
                        stack.push((child, e.id));
                    }
                } else if prefixes.in_scope(b) {
                    out.insert(
                        child,
                        EntryInfo {
                            mode: e.mode,
                            id: e.id,
                            size: e.size,
                        },
                    );
                }
            }
        }
        Ok(out)
    }
}

fn load_roots(path: &Path) -> BTreeMap<ObjectId, ObjectId> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(c), Some(t)) = (parts.next(), parts.next())
            && let (Ok(c), Ok(t)) = (ObjectId::parse_text(c), ObjectId::parse_text(t))
            && c.kind() == Kind::Changeset
            && t.kind() == Kind::Tree
        {
            out.insert(c, t);
        }
    }
    out
}

/// A status [`TreeLookup`] over the local tree cache (SPEC 11.5: zero RPCs when trees are
/// cached). Trees must have been made available beforehand (`WorkingCopy::status` fetches any
/// missing ones first).
pub(crate) struct CachedTreeLookup<'a> {
    pub(crate) cache: &'a TreeCache,
    pub(crate) root: ObjectId,
    pub(crate) inaccessible: Vec<RepoPath>,
}

impl TreeLookup for CachedTreeLookup<'_> {
    fn files_in_scope(&self, scope: &[RepoPath]) -> Result<BTreeMap<RepoPath, EntryInfo>> {
        self.cache
            .files_in_scope(&self.root, scope, &self.inaccessible)
            .map_err(|e| {
                if e.code == ErrorCode::NotFound {
                    ScionError::unavailable(
                        scion_core::subcode::UPSTREAM_UNAVAILABLE,
                        format!("base tree not cached locally: {}", e.message),
                    )
                } else {
                    e
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_object::{Extensions, Name, TreeEntry};

    #[test]
    fn insert_get_roots_and_scope_walk() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TreeCache::open(dir.path()).unwrap();
        let blob = ObjectId::hash_blob(b"x");
        let sub = Tree {
            entries: vec![TreeEntry {
                name: Name::new(b"f").unwrap(),
                mode: Mode::File,
                id: blob,
                size: 1,
            }],
            extensions: Extensions::new(),
        };
        let sub_raw = Bytes::from(sub.encode());
        let sub_id = sub.id();
        let root = Tree {
            entries: vec![
                TreeEntry {
                    name: Name::new(b"a").unwrap(),
                    mode: Mode::Tree,
                    id: sub_id,
                    size: 0,
                },
                TreeEntry {
                    name: Name::new(b"top").unwrap(),
                    mode: Mode::File,
                    id: blob,
                    size: 1,
                },
            ],
            extensions: Extensions::new(),
        };
        let root_id = root.id();
        assert!(!cache.contains(&sub_id));
        // Wrong id: integrity.
        assert_eq!(
            cache
                .insert_verified(&root_id, sub_raw.clone())
                .unwrap_err()
                .code,
            ErrorCode::Integrity
        );
        cache.insert_verified(&sub_id, sub_raw).unwrap();
        cache
            .insert_verified(&root_id, Bytes::from(root.encode()))
            .unwrap();
        let all = cache
            .files_in_scope(&root_id, &[RepoPath::root()], &[])
            .unwrap();
        assert_eq!(all.len(), 2);
        let scoped = cache
            .files_in_scope(&root_id, &[RepoPath::parse(b"a").unwrap()], &[])
            .unwrap();
        assert_eq!(
            scoped.keys().cloned().collect::<Vec<_>>(),
            vec![RepoPath::parse(b"a/f").unwrap()]
        );
        let hidden = cache
            .files_in_scope(
                &root_id,
                &[RepoPath::root()],
                &[RepoPath::parse(b"a").unwrap()],
            )
            .unwrap();
        assert_eq!(hidden.len(), 1);

        let cs = ObjectId::new(Kind::Changeset, [3; 32]);
        cache.set_root(&cs, &root_id).unwrap();
        drop(cache);
        let reopened = TreeCache::open(dir.path()).unwrap();
        assert_eq!(reopened.root_of(&cs), Some(root_id));
        assert!(reopened.contains(&sub_id));
        assert_eq!(reopened.get(&root_id).unwrap().unwrap().entries.len(), 2);
        assert!(reopened.contains(&empty_tree_id()));
    }
}
