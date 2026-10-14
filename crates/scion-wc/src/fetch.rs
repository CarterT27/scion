//! SPEC 10.1 fetch walk: breadth-first, scope-pruned, verified, resumable (P18).
//!
//! Trees are requested level by level (breadth-first by depth), only when missing from the
//! local tree cache and only within the sparse scope (a subtree is entered when it is in scope
//! or on the way to a scope prefix, SPEC 11.2). A `denied` subtree is recorded as inaccessible
//! (SPEC 12.6) and not entered. Blobs are fetched afterwards by the caller, in batches, through
//! [`crate::remote::Fetcher`], which verifies and resumes.
//!
//! [`diff_scope`] is the pruned two-tree walk of `goto` (SPEC 11.3): identical subtree ids are
//! never entered, so only differing trees are fetched.

use crate::remote::{Fetcher, Item};
use crate::status::Prefixes;
use crate::tree_cache::TreeCache;
use scion_core::{EntryInfo, Mode, ObjectId, Result, ScionError};
use scion_object::{RepoPath, Tree, TreeEntry};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The in-scope view of one tree (SPEC 11.2).
#[derive(Clone, Debug, Default)]
pub(crate) struct Listing {
    /// Every file entry in scope (inaccessible prefixes excluded).
    pub files: BTreeMap<RepoPath, EntryInfo>,
    /// Directories that are in scope or on the way to a scope prefix.
    pub dirs: BTreeSet<RepoPath>,
    /// Subtrees found `denied` during this walk (SPEC 12.6).
    pub denied: Vec<RepoPath>,
}

/// One file-level difference between two trees within scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    pub path: RepoPath,
    pub old: Option<EntryInfo>,
    pub new: Option<EntryInfo>,
}

/// Make every tree in `ids` available locally, fetching the missing ones in batches. Returns
/// the ids that upstream refused (`denied`, or `redacted`, which trees cannot legitimately be).
/// A missing tree is NOT_FOUND; a tree that fails SPEC 4 validation (for example a malicious
/// `..` or `.scion` name) is refused with INVALID_ARGUMENT before anything is written.
pub(crate) async fn ensure_trees(
    fetcher: &Fetcher,
    cache: &TreeCache,
    ids: &[ObjectId],
) -> Result<BTreeSet<ObjectId>> {
    let mut seen = BTreeSet::new();
    let missing: Vec<ObjectId> = ids
        .iter()
        .copied()
        .filter(|id| !cache.contains(id) && seen.insert(*id))
        .collect();
    let mut refused = BTreeSet::new();
    if missing.is_empty() {
        return Ok(refused);
    }
    for (id, item) in fetcher.get_objects(&missing).await? {
        match item {
            Item::Ok(raw) => {
                cache.insert_verified(&id, raw)?;
            }
            Item::Denied | Item::Redacted => {
                refused.insert(id);
            }
            Item::NotFound => {
                return Err(ScionError::not_found(format!(
                    "tree {id} not found upstream"
                )));
            }
        }
    }
    Ok(refused)
}

/// SPEC 10.1 steps 1-2: breadth-first listing of `root` within `scope`, fetching only missing
/// trees, skipping out-of-scope subtrees and known `inaccessible` prefixes. Zero RPCs when
/// every needed tree is cached.
pub(crate) async fn list_scope(
    fetcher: &Fetcher,
    cache: &TreeCache,
    root: &ObjectId,
    scope: &[RepoPath],
    inaccessible: &[RepoPath],
) -> Result<Listing> {
    let prefixes = Prefixes::new(scope, inaccessible);
    let mut listing = Listing::default();
    let mut level: Vec<(RepoPath, ObjectId)> = vec![(RepoPath::root(), *root)];
    while !level.is_empty() {
        let ids: Vec<ObjectId> = level.iter().map(|(_, id)| *id).collect();
        let refused = ensure_trees(fetcher, cache, &ids).await?;
        let mut next = Vec::new();
        for (path, id) in level {
            if refused.contains(&id) {
                if path.is_root() {
                    return Err(ScionError::permission_denied(
                        "the root tree is not readable",
                    ));
                }
                listing.dirs.remove(&path);
                listing.denied.push(path);
                continue;
            }
            let tree = cache.require(&id)?;
            for e in &tree.entries {
                let child = path.join(&e.name)?;
                let b = child.as_bytes();
                if prefixes.inaccessible(b) {
                    continue;
                }
                if e.mode == Mode::Tree {
                    if prefixes.in_scope(b) || prefixes.leads_to_scope(b) {
                        listing.dirs.insert(child.clone());
                        next.push((child, e.id));
                    }
                } else if prefixes.in_scope(b) {
                    listing.files.insert(child, info(e.mode, e.id, e.size));
                }
            }
        }
        level = next;
    }
    listing.denied.sort();
    Ok(listing)
}

fn info(mode: Mode, id: ObjectId, size: u64) -> EntryInfo {
    EntryInfo { mode, id, size }
}

/// Old and new entries of one name.
type EntryPair<'a> = (Option<&'a TreeEntry>, Option<&'a TreeEntry>);

/// A pair of directory trees to compare at `path` (`None` = absent on that side).
struct Pair {
    path: RepoPath,
    old: Option<ObjectId>,
    new: Option<ObjectId>,
}

/// Result of [`diff_scope`].
#[derive(Debug, Default)]
pub(crate) struct ScopeDiff {
    /// File-level changes, ascending by path.
    pub changes: Vec<Change>,
    /// Subtrees of the new tree found `denied` (SPEC 12.6).
    pub denied: Vec<RepoPath>,
    /// Directory paths whose subtrees were compared (used to update inaccessible prefixes).
    pub visited: BTreeSet<RepoPath>,
}

/// SPEC 11.3 pruned diff of `old_root` and `new_root` within `scope`: identical subtree ids
/// are skipped, so only differing trees are fetched. Old-side content under known
/// `inaccessible` prefixes was never materialized and counts as absent.
pub(crate) async fn diff_scope(
    fetcher: &Fetcher,
    cache: &TreeCache,
    old_root: &ObjectId,
    new_root: &ObjectId,
    scope: &[RepoPath],
    inaccessible: &[RepoPath],
) -> Result<ScopeDiff> {
    let scope_p = Prefixes::new(scope, &[]);
    let inacc_p = Prefixes::new(scope, inaccessible);
    let mut out = ScopeDiff::default();
    if old_root == new_root {
        return Ok(out);
    }
    let mut level = vec![Pair {
        path: RepoPath::root(),
        old: Some(*old_root),
        new: Some(*new_root),
    }];
    while !level.is_empty() {
        let mut ids = Vec::new();
        for p in &level {
            if let Some(o) = p.old
                && !inacc_p.inaccessible(p.path.as_bytes())
            {
                ids.push(o);
            }
            if let Some(n) = p.new {
                ids.push(n);
            }
        }
        let refused = ensure_trees(fetcher, cache, &ids).await?;
        let mut next = Vec::new();
        for p in level {
            out.visited.insert(p.path.clone());
            let new_tree: Option<Arc<Tree>> = match p.new {
                Some(n) if refused.contains(&n) => {
                    if p.path.is_root() {
                        return Err(ScionError::permission_denied(
                            "the root tree is not readable",
                        ));
                    }
                    // The new subtree is not readable (SPEC 12.6): it is recorded as
                    // inaccessible and, as in a fresh checkout, not materialized, so the old
                    // files under it are removed (subject to goto's dirty-file protection).
                    out.denied.push(p.path.clone());
                    None
                }
                Some(n) => Some(cache.require(&n)?),
                None => None,
            };
            let old_tree: Option<Arc<Tree>> = match p.old {
                Some(o) if inacc_p.inaccessible(p.path.as_bytes()) || refused.contains(&o) => None,
                Some(o) => Some(cache.require(&o)?),
                None => None,
            };
            let mut names: BTreeMap<&[u8], EntryPair<'_>> = BTreeMap::new();
            if let Some(t) = &old_tree {
                for e in &t.entries {
                    names.entry(e.name.as_bytes()).or_default().0 = Some(e);
                }
            }
            if let Some(t) = &new_tree {
                for e in &t.entries {
                    names.entry(e.name.as_bytes()).or_default().1 = Some(e);
                }
            }
            for (o, n) in names.into_values() {
                let name = match (o, n) {
                    (Some(e), _) | (None, Some(e)) => &e.name,
                    (None, None) => continue,
                };
                let child = p.path.join(name)?;
                let b = child.as_bytes();
                if !scope_p.in_scope(b) && !scope_p.leads_to_scope(b) {
                    continue;
                }
                let old_dir = o.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                let new_dir = n.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                if old_dir != new_dir {
                    next.push(Pair {
                        path: child.clone(),
                        old: old_dir,
                        new: new_dir,
                    });
                }
                if !scope_p.in_scope(b) {
                    continue;
                }
                // Files under a known inaccessible prefix were never materialized.
                let old_file = o
                    .filter(|e| e.mode != Mode::Tree && !inacc_p.inaccessible(b))
                    .map(|e| info(e.mode, e.id, e.size));
                let new_file = n
                    .filter(|e| e.mode != Mode::Tree)
                    .map(|e| info(e.mode, e.id, e.size));
                if old_file != new_file {
                    out.changes.push(Change {
                        path: child,
                        old: old_file,
                        new: new_file,
                    });
                }
            }
        }
        level = next;
    }
    out.changes.sort_by(|a, b| a.path.cmp(&b.path));
    out.denied.sort();
    Ok(out)
}
