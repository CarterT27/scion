//! Reading git repositories with gix (pure Rust; SPEC 14): commit walk, tree diff mapped to
//! Scion modes, gitlink scan.

use crate::mapping::{GIT_MODE_GITLINK, GIT_MODE_TREE, map_mode};
use scion_core::{ErrorCode, Mode, Result, ScionError};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// A git object id (SHA-1).
pub(crate) type GitOid = gix::ObjectId;

/// A gitlink found in a tree: `(path bytes, 20-byte commit sha)`.
pub(crate) type Gitlink = (Vec<u8>, [u8; 20]);

fn invalid(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, msg)
}

fn not_found(msg: impl Into<String>) -> ScionError {
    ScionError::not_found(msg)
}

/// The 20 raw bytes of a SHA-1 object id.
pub(crate) fn oid_bytes(id: &GitOid) -> Result<[u8; 20]> {
    <[u8; 20]>::try_from(id.as_bytes())
        .map_err(|_| invalid(format!("object id {id} is not a 20-byte SHA-1")))
}

/// One commit: id, parents in order, root tree and the raw object bytes.
#[derive(Clone, Debug)]
pub(crate) struct CommitInfo {
    /// Commit id.
    pub id: GitOid,
    /// Parent ids in order.
    pub parents: Vec<GitOid>,
    /// Root tree id.
    pub tree: GitOid,
    /// The raw commit object (headers and message).
    pub raw: Vec<u8>,
}

/// One raw tree entry.
#[derive(Clone, Debug)]
pub(crate) struct RawEntry {
    /// Entry name.
    pub name: Vec<u8>,
    /// Git mode as written (e.g. `0o100664`; trees read as `0o040000`).
    pub mode: u32,
    /// Object id.
    pub oid: GitOid,
}

/// A file-level change between two git trees, in Scion terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// The path holds a file with this Scion mode and git blob.
    Put {
        /// Scion mode (1..=3).
        mode: Mode,
        /// Git blob id.
        oid: GitOid,
    },
    /// The path no longer holds a file.
    Delete,
}

/// What one tree entry is, for diffing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Absent,
    Tree(GitOid),
    File(Mode, GitOid),
    /// Gitlinks are invisible to Scion (skipped or rejected elsewhere).
    Gitlink,
}

fn classify(e: &RawEntry, path: &[u8]) -> Result<Side> {
    if e.mode == GIT_MODE_GITLINK {
        return Ok(Side::Gitlink);
    }
    if e.mode == GIT_MODE_TREE {
        return Ok(Side::Tree(e.oid));
    }
    match map_mode(e.mode) {
        Ok(Some(m)) => Ok(Side::File(m, e.oid)),
        Ok(None) => Ok(Side::Tree(e.oid)),
        Err(err) => Err(invalid(format!(
            "{} (path {:?})",
            err.message,
            String::from_utf8_lossy(path)
        ))),
    }
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

/// A git repository opened with gix.
pub(crate) struct GitRepo {
    repo: gix::Repository,
    /// Gitlinks below each tree already scanned (relative paths).
    gitlink_memo: HashMap<GitOid, Arc<Vec<Gitlink>>>,
}

impl GitRepo {
    /// Open the repository at `path` (bare or with a work tree).
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).map_err(|e| {
            invalid(format!(
                "cannot open git repository {}: {e}",
                path.display()
            ))
        })?;
        Ok(GitRepo {
            repo,
            gitlink_memo: HashMap::new(),
        })
    }

    /// Read one object.
    pub fn read(&self, id: GitOid) -> Result<(gix::objs::Kind, Vec<u8>)> {
        let obj = self
            .repo
            .find_object(id)
            .map_err(|e| not_found(format!("git object {id}: {e}")))?;
        Ok((obj.kind, obj.data.clone()))
    }

    /// Resolve a revision (`main`, `refs/heads/x`, `HEAD`, a sha, `main~2`, a tag) to the
    /// commit it denotes, peeling annotated tags.
    pub fn resolve_commit(&self, spec: &str) -> Result<GitOid> {
        let id = self
            .repo
            .rev_parse_single(spec)
            .map_err(|e| not_found(format!("git ref {spec:?} does not resolve: {e}")))?
            .detach();
        let mut id = id;
        for _ in 0..32 {
            let (kind, data) = self.read(id)?;
            match kind {
                gix::objs::Kind::Commit => return Ok(id),
                gix::objs::Kind::Tag => {
                    let tag = gix::objs::TagRef::from_bytes(&data, gix::hash::Kind::Sha1)
                        .map_err(|e| invalid(format!("bad git tag {id}: {e}")))?;
                    id = tag.target();
                }
                other => {
                    return Err(invalid(format!(
                        "git ref {spec:?} denotes a {other}, not a commit"
                    )));
                }
            }
        }
        Err(invalid(format!("git ref {spec:?}: tag chain too long")))
    }

    /// Read and parse a commit.
    pub fn commit(&self, id: GitOid) -> Result<CommitInfo> {
        let (kind, raw) = self.read(id)?;
        if kind != gix::objs::Kind::Commit {
            return Err(invalid(format!(
                "git object {id} is a {kind}, not a commit"
            )));
        }
        let (parents, tree) = {
            let c = gix::objs::CommitRef::from_bytes(&raw, gix::hash::Kind::Sha1)
                .map_err(|e| invalid(format!("bad git commit {id}: {e}")))?;
            (c.parents().collect::<Vec<GitOid>>(), c.tree())
        };
        Ok(CommitInfo {
            id,
            parents,
            tree,
            raw,
        })
    }

    /// The entries of a tree.
    pub fn tree_entries(&self, id: GitOid) -> Result<Vec<RawEntry>> {
        let (kind, data) = self.read(id)?;
        if kind != gix::objs::Kind::Tree {
            return Err(invalid(format!("git object {id} is a {kind}, not a tree")));
        }
        let tree = gix::objs::TreeRef::from_bytes(&data, gix::hash::Kind::Sha1)
            .map_err(|e| invalid(format!("bad git tree {id}: {e}")))?;
        Ok(tree
            .entries
            .iter()
            .map(|e| RawEntry {
                name: e.filename.to_vec(),
                mode: u32::from(e.mode.value()),
                oid: e.oid.to_owned(),
            })
            .collect())
    }

    /// Blob content.
    pub fn blob(&self, id: GitOid) -> Result<Vec<u8>> {
        let (kind, data) = self.read(id)?;
        if kind != gix::objs::Kind::Blob {
            return Err(invalid(format!("git object {id} is a {kind}, not a blob")));
        }
        Ok(data)
    }

    /// Every commit reachable from `tip`, parents before children (SPEC 14 topological
    /// order). Deterministic: parents are visited in order (iterative post-order DFS).
    pub fn topo_order(&self, tip: GitOid) -> Result<Vec<CommitInfo>> {
        let mut out = Vec::new();
        let mut done: HashSet<GitOid> = HashSet::new();
        let mut stack: Vec<(GitOid, Option<CommitInfo>)> = vec![(tip, None)];
        while let Some((id, info)) = stack.pop() {
            if done.contains(&id) {
                continue;
            }
            match info {
                Some(c) => {
                    done.insert(id);
                    out.push(c);
                }
                None => {
                    let c = self.commit(id)?;
                    let parents = c.parents.clone();
                    stack.push((id, Some(c)));
                    for p in parents.iter().rev() {
                        if !done.contains(p) {
                            stack.push((*p, None));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// The file-level changes from tree `a` (`None` = empty) to tree `b`, in Scion terms,
    /// ascending by path bytes (SPEC 4.3.4 FileDiff over the mapped trees).
    ///
    /// Identical subtrees are skipped by id. Files compare by (Scion mode, blob id), so a
    /// 100644 <-> 100664 flip is no change. Gitlinks count as absent; subtrees that end up with
    /// no files produce nothing (Scion has no empty directories).
    pub fn diff_trees(
        &self,
        a: Option<GitOid>,
        b: Option<GitOid>,
    ) -> Result<Vec<(Vec<u8>, Change)>> {
        let mut out: Vec<(Vec<u8>, Change)> = Vec::new();
        let mut stack: Vec<(Vec<u8>, Option<GitOid>, Option<GitOid>)> = vec![(Vec::new(), a, b)];
        while let Some((prefix, a, b)) = stack.pop() {
            if a == b {
                continue;
            }
            let side_map = |t: Option<GitOid>| -> Result<BTreeMap<Vec<u8>, Side>> {
                let mut m = BTreeMap::new();
                if let Some(t) = t {
                    for e in self.tree_entries(t)? {
                        let path = join(&prefix, &e.name);
                        let s = classify(&e, &path)?;
                        m.insert(e.name, s);
                    }
                }
                Ok(m)
            };
            let ma = side_map(a)?;
            let mb = side_map(b)?;
            let mut names: Vec<&Vec<u8>> = ma.keys().chain(mb.keys()).collect();
            names.sort();
            names.dedup();
            for name in names {
                let sa = ma.get(name).copied().unwrap_or(Side::Absent);
                let sb = mb.get(name).copied().unwrap_or(Side::Absent);
                let path = join(&prefix, name);
                match (sa, sb) {
                    (Side::Tree(x), Side::Tree(y)) => stack.push((path, Some(x), Some(y))),
                    (Side::Tree(x), other) => {
                        stack.push((path.clone(), Some(x), None));
                        if let Side::File(mode, oid) = other {
                            out.push((path, Change::Put { mode, oid }));
                        }
                    }
                    (other, Side::Tree(y)) => {
                        if let Side::File(..) = other {
                            out.push((path.clone(), Change::Delete));
                        }
                        stack.push((path, None, Some(y)));
                    }
                    (Side::File(ma_, oa), Side::File(mb_, ob)) => {
                        if ma_ != mb_ || oa != ob {
                            out.push((path, Change::Put { mode: mb_, oid: ob }));
                        }
                    }
                    (Side::File(..), Side::Absent | Side::Gitlink) => {
                        out.push((path, Change::Delete));
                    }
                    (Side::Absent | Side::Gitlink, Side::File(mode, oid)) => {
                        out.push((path, Change::Put { mode, oid }));
                    }
                    (Side::Absent | Side::Gitlink, Side::Absent | Side::Gitlink) => {}
                }
            }
        }
        out.sort_by(|x, y| x.0.cmp(&y.0));
        Ok(out)
    }

    /// Every gitlink in tree `t`, ascending by path (memoized per tree id).
    pub fn gitlinks(&mut self, t: GitOid) -> Result<Arc<Vec<Gitlink>>> {
        // Iterative post-order over subtrees not yet memoized.
        let mut stack: Vec<(GitOid, bool)> = vec![(t, false)];
        while let Some((id, expanded)) = stack.pop() {
            if self.gitlink_memo.contains_key(&id) {
                continue;
            }
            let entries = self.tree_entries(id)?;
            if expanded {
                let mut links: Vec<Gitlink> = Vec::new();
                for e in &entries {
                    if e.mode == GIT_MODE_GITLINK {
                        links.push((e.name.clone(), oid_bytes(&e.oid)?));
                    } else if e.mode == GIT_MODE_TREE {
                        let sub = self.gitlink_memo.get(&e.oid).cloned().ok_or_else(|| {
                            ScionError::new(ErrorCode::Unavailable, "gitlink scan order")
                        })?;
                        for (p, sha) in sub.iter() {
                            links.push((join(&e.name, p), *sha));
                        }
                    }
                }
                links.sort_by(|x, y| x.0.cmp(&y.0));
                self.gitlink_memo.insert(id, Arc::new(links));
            } else {
                stack.push((id, true));
                for e in &entries {
                    if e.mode == GIT_MODE_TREE && !self.gitlink_memo.contains_key(&e.oid) {
                        stack.push((e.oid, false));
                    }
                }
            }
        }
        self.gitlink_memo
            .get(&t)
            .cloned()
            .ok_or_else(|| ScionError::new(ErrorCode::Unavailable, "gitlink scan incomplete"))
    }
}
