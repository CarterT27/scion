//! Local tree cache under `.scion/cache` (scion-cache DiskCache; SPEC 10.1, 11.5; P18).
//!
//! Trees are cached exactly as verified (raw canonical bytes) in a [`DiskCache`] so `status`,
//! `goto` and `sparse` can walk the base tree without contacting the server (SPEC 11.5: status
//! MUST NOT contact the server when every needed tree is cached). Decoded trees are memoized
//! for the lifetime of the handle. The changeset -> root tree mapping is kept in
//! `.scion/cache/roots` so the base root tree is known without a `GetChangesets` call.

use crate::fs_safe::write_atomic;
use bytes::Bytes;
use scion_cache::{DiskCache, Payload};
use scion_core::{Kind, ObjectId, Result, ScionError, empty_tree_id};
use scion_object::{Canonical, Tree};
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

    /// Whether `id` is available locally and intact (the empty tree always is).
    ///
    /// A disk entry is read, digest-verified and decoded (then memoized), so a corrupt entry is
    /// discarded here and reported absent; callers such as `fetch::ensure_trees` then re-fetch
    /// it within the same command (SPEC 16.1 class 7 at the client tier). Consulting only the
    /// index would let a later `require` fail NOT_FOUND on the dropped entry.
    pub(crate) fn contains(&self, id: &ObjectId) -> bool {
        if *id == empty_tree_id() || lock(&self.mem).contains_key(id) {
            return true;
        }
        if !self.disk.contains(id) {
            return false;
        }
        match self.get(id) {
            Ok(t) => t.is_some(),
            Err(e) => {
                tracing::warn!(object = %id, error = %e, "cached tree unreadable; re-fetching");
                false
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{ErrorCode, Mode};
    use scion_object::{Extensions, Name, TreeEntry};

    #[test]
    fn insert_get_and_roots() {
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
        assert_eq!(cache.require(&root_id).unwrap().entries.len(), 2);
        assert_eq!(cache.require(&sub_id).unwrap().entries.len(), 1);

        let cs = ObjectId::new(Kind::Changeset, [3; 32]);
        cache.set_root(&cs, &root_id).unwrap();
        drop(cache);
        let reopened = TreeCache::open(dir.path()).unwrap();
        assert_eq!(reopened.root_of(&cs), Some(root_id));
        assert!(reopened.contains(&sub_id));
        assert_eq!(reopened.get(&root_id).unwrap().unwrap().entries.len(), 2);
        assert!(reopened.contains(&empty_tree_id()));
    }

    #[test]
    fn contains_discards_a_corrupt_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TreeCache::open(dir.path()).unwrap();
        let tree = Tree {
            entries: vec![TreeEntry {
                name: Name::new(b"f").unwrap(),
                mode: Mode::File,
                id: ObjectId::hash_blob(b"x"),
                size: 1,
            }],
            extensions: Extensions::new(),
        };
        let id = tree.id();
        cache
            .insert_verified(&id, Bytes::from(tree.encode()))
            .unwrap();
        drop(cache);
        // Flip the last byte of the stored file (bypassing the index).
        let hex = id.hex();
        let file = dir
            .path()
            .join(CACHE_DIR)
            .join("objects")
            .join("t")
            .join(&hex[..2])
            .join(&hex);
        let mut bytes = std::fs::read(&file).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        std::fs::write(&file, bytes).unwrap();
        let reopened = TreeCache::open(dir.path()).unwrap();
        assert!(reopened.disk.contains(&id), "the index still lists it");
        assert!(!reopened.contains(&id), "a corrupt entry is not contained");
        assert!(!reopened.disk.contains(&id), "and it was discarded");
        assert!(reopened.get(&id).unwrap().is_none());
    }
}
