//! The model commit graph (SPEC 8.2, 9.2, 18.5).
//!
//! Deliberately naive: every query walks the whole reachable graph. Commits are indexed in
//! insertion order and a commit's parents must already exist, so parents always have smaller
//! indices (parent indices that do not satisfy this are ignored by every query).

use crate::model::is_under;
use crate::{MConflict, MFile, MTree, diff, file_diff, merge_trees};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Why a model rebase failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MRebaseError {
    /// Conflicts (SPEC 8.3).
    Conflict(Vec<MConflict>),
    /// EMPTY_CHANGE (SPEC 8.2).
    Empty,
}

/// A naive in-memory repository; commits are indexed `0..n` in insertion order.
#[derive(Clone, Debug, Default)]
pub struct MRepo {
    commits: Vec<(Vec<usize>, MTree)>,
}

static EMPTY_TREE: MTree = MTree::new();

/// The content of a path for SPEC 9.2 node reuse: a file's bytes (the blob id; the mode is not
/// part of it) or a directory's files (its tree id).
#[derive(Clone, Debug, PartialEq, Eq)]
enum PathContent {
    File(Vec<u8>),
    Dir(Vec<(Vec<u8>, MFile)>),
}

fn path_content(t: &MTree, p: &[u8]) -> Option<PathContent> {
    if let Some(f) = t.get(p) {
        return Some(PathContent::File(f.content.clone()));
    }
    let below: Vec<(Vec<u8>, MFile)> = t
        .iter()
        .filter(|(k, _)| is_under(p, k))
        .map(|(k, f)| (k.clone(), f.clone()))
        .collect();
    if below.is_empty() && !p.is_empty() {
        None
    } else {
        Some(PathContent::Dir(below))
    }
}

impl MRepo {
    /// An empty repository.
    pub fn new() -> Self {
        MRepo {
            commits: Vec::new(),
        }
    }

    /// Add a commit with the given parents and tree; returns its index.
    pub fn commit(&mut self, parents: &[usize], tree: MTree) -> usize {
        self.commits.push((parents.to_vec(), tree));
        self.commits.len() - 1
    }

    /// Number of commits.
    pub fn len(&self) -> usize {
        self.commits.len()
    }

    /// True when there are no commits.
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }

    /// The parents of commit `c`, in order (valid indices only).
    pub fn parents(&self, c: usize) -> Vec<usize> {
        match self.commits.get(c) {
            Some((ps, _)) => ps.iter().copied().filter(|&p| p < c).collect(),
            None => Vec::new(),
        }
    }

    /// The tree of commit `c` (the empty tree for an unknown index).
    pub fn tree(&self, c: usize) -> &MTree {
        match self.commits.get(c) {
            Some((_, t)) => t,
            None => &EMPTY_TREE,
        }
    }

    /// SPEC 9.2 generation (root = 1): `1 + max(parent generations)`.
    pub fn generation(&self, c: usize) -> u64 {
        self.generations().get(c).copied().unwrap_or(0)
    }

    /// Generations of all commits, by index.
    fn generations(&self) -> Vec<u64> {
        let mut gens: Vec<u64> = Vec::new();
        for i in 0..self.commits.len() {
            let g = 1 + self
                .parents(i)
                .iter()
                .map(|&p| gens.get(p).copied().unwrap_or(0))
                .max()
                .unwrap_or(0);
            gens.push(g);
        }
        gens
    }

    /// The first-parent chain of `c`, starting with `c`.
    fn first_parent_chain(&self, c: usize) -> Vec<usize> {
        let mut out = Vec::new();
        if c >= self.commits.len() {
            return out;
        }
        let mut cur = c;
        out.push(cur);
        while let Some(&p) = self.parents(cur).first() {
            out.push(p);
            cur = p;
        }
        out
    }

    /// SPEC 9.2 skip pointers: first-parent ancestors at distance 2^k, for k while one exists.
    pub fn skip(&self, c: usize) -> Vec<usize> {
        let chain = self.first_parent_chain(c);
        let mut out = Vec::new();
        let mut dist = 1usize;
        while let Some(&a) = chain.get(dist) {
            out.push(a);
            dist *= 2;
        }
        out
    }

    /// All ancestors of `c`, including `c` itself (full BFS).
    pub fn ancestors(&self, c: usize) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        if c >= self.commits.len() {
            return seen;
        }
        let mut queue = VecDeque::from([c]);
        while let Some(x) = queue.pop_front() {
            if seen.insert(x) {
                queue.extend(self.parents(x));
            }
        }
        seen
    }

    /// Is `a` an ancestor of (or equal to) `b` (full BFS; SPEC 18.5).
    pub fn is_ancestor(&self, a: usize, b: usize) -> bool {
        self.ancestors(b).contains(&a)
    }

    /// All best common ancestors, ascending by index: the common ancestors (inclusive) that are
    /// not a proper ancestor of another common ancestor (SPEC 8.3 `Merge`).
    pub fn merge_bases(&self, a: usize, b: usize) -> Vec<usize> {
        let aa = self.ancestors(a);
        let bb = self.ancestors(b);
        let common: Vec<usize> = aa.intersection(&bb).copied().collect();
        common
            .iter()
            .copied()
            .filter(|&c| !common.iter().any(|&d| d != c && self.is_ancestor(c, d)))
            .collect()
    }

    /// SPEC 8.2 rebase of `c` onto `onto`.
    ///
    /// `P = parents[0]` of `c` (the empty tree for a root; v1 callers guard `multi_parent`
    /// themselves). Fast path `onto == P` returns `c`. Otherwise the delta paths of `c` are
    /// merged with [`merge_trees`] (`B = tree(P)`, `O = tree(onto)`, `T = tree(c)`), an
    /// unchanged result is [`MRebaseError::Empty`], and a new commit with parents `[onto]` is
    /// added and returned.
    pub fn rebase(&mut self, c: usize, onto: usize, max_text: u64) -> Result<usize, MRebaseError> {
        let p = self.parents(c).first().copied();
        if p == Some(onto) {
            return Ok(c);
        }
        let b = match p {
            Some(p) => self.tree(p).clone(),
            None => MTree::new(),
        };
        let o = self.tree(onto).clone();
        let t = self.tree(c).clone();
        let paths = file_diff(&b, &t)
            .into_iter()
            .map(|d| match d {
                crate::MDelta::Put(p, _) => p,
                crate::MDelta::Delete(p) => p,
            })
            .collect();
        let merged = merge_trees(&b, &o, &t, Some(&paths), max_text, &BTreeSet::new())
            .map_err(MRebaseError::Conflict)?;
        if merged == o {
            return Err(MRebaseError::Empty);
        }
        Ok(self.commit(&[onto], merged))
    }

    /// The commit that created the SPEC 9.2 history node of `p` at `c`: follow `parents[0]`
    /// while the path's content there is equal; `None` when `p` does not exist at `c`.
    fn node_of(&self, c: usize, p: &[u8]) -> Option<usize> {
        let mut cur = c;
        let mut content = path_content(self.tree(cur), p)?;
        loop {
            match self.parents(cur).first() {
                Some(&p0) => match path_content(self.tree(p0), p) {
                    Some(pc) if pc == content => {
                        cur = p0;
                        content = pc;
                    }
                    _ => return Some(cur),
                },
                None => return Some(cur),
            }
        }
    }

    /// Parent nodes of the node created at `n` for `p`: the nodes of `p` in each parent of `n`
    /// where it exists, deduplicated, in parent order (SPEC 9.2).
    fn node_parents(&self, n: usize, p: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        for parent in self.parents(n) {
            if let Some(x) = self.node_of(parent, p)
                && !out.contains(&x)
            {
                out.push(x);
            }
        }
        out
    }

    /// Log of path `p` from `from` with TREESAME semantics equivalent to SPEC 9.2 node reuse.
    ///
    /// The result is the creating commit of every history node reachable from the node of `p`
    /// at `from` (empty when `p` does not exist there), newest first: generation descending,
    /// ties by index descending. A node's content is the file's bytes (mode excluded, as the
    /// node stores only the blob id) or the directory's tree. `first_parent_only` follows only
    /// each node's first parent node.
    pub fn log_path(&self, from: usize, p: &[u8], first_parent_only: bool) -> Vec<usize> {
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        let mut queue: VecDeque<usize> = self.node_of(from, p).into_iter().collect();
        while let Some(n) = queue.pop_front() {
            if !seen.insert(n) {
                continue;
            }
            let ps = self.node_parents(n, p);
            if first_parent_only {
                queue.extend(ps.first());
            } else {
                queue.extend(ps);
            }
        }
        let gens = self.generations();
        let gen_of = |c: usize| gens.get(c).copied().unwrap_or(0);
        let mut out: Vec<usize> = seen.into_iter().collect();
        out.sort_by(|a, b| gen_of(*b).cmp(&gen_of(*a)).then(b.cmp(a)));
        out
    }

    /// SPEC 9.2 blame: `(start_line, line_count, origin_commit, origin_start_line)`.
    ///
    /// Lines are 0-based. Runs merge adjacent lines with the same origin commit and consecutive
    /// origin lines. Empty when `p` is not a file at `c`.
    pub fn blame(&self, c: usize, p: &[u8]) -> Vec<(u32, u32, usize, u32)> {
        self.blame_redacted(c, p, &BTreeSet::new())
    }

    /// SPEC 9.2 / 13.5 blame where every content in `redacted` is treated as empty.
    pub fn blame_redacted(
        &self,
        c: usize,
        p: &[u8],
        redacted: &BTreeSet<Vec<u8>>,
    ) -> Vec<(u32, u32, usize, u32)> {
        let Some(n) = self.node_of(c, p) else {
            return Vec::new();
        };
        if !self.tree(n).contains_key(p) {
            return Vec::new();
        }
        let lines = self.blame_lines(n, p, redacted);
        let mut out: Vec<(u32, u32, usize, u32)> = Vec::new();
        for (i, (origin, oline)) in lines.into_iter().enumerate() {
            let i = i as u32;
            if let Some(last) = out.last_mut()
                && last.2 == origin
                && last.0 + last.1 == i
                && last.3 + last.1 == oline
            {
                last.1 += 1;
                continue;
            }
            out.push((i, 1, origin, oline));
        }
        out
    }

    /// The file content of `p` at commit `n` for blame (redacted or non-file content is empty).
    fn blame_content(&self, n: usize, p: &[u8], redacted: &BTreeSet<Vec<u8>>) -> Vec<u8> {
        match self.tree(n).get(p) {
            Some(f) if !redacted.contains(&f.content) => f.content.clone(),
            _ => Vec::new(),
        }
    }

    /// Per-line `(origin_commit, origin_line)` of the node created at `n` for `p`. Computed
    /// iteratively over the reachable nodes in index order (parents before children).
    fn blame_lines(&self, n: usize, p: &[u8], redacted: &BTreeSet<Vec<u8>>) -> Vec<(usize, u32)> {
        let mut nodes: BTreeSet<usize> = BTreeSet::new();
        let mut queue = VecDeque::from([n]);
        while let Some(x) = queue.pop_front() {
            if nodes.insert(x) {
                queue.extend(self.node_parents(x, p));
            }
        }
        let mut memo: BTreeMap<usize, Vec<(usize, u32)>> = BTreeMap::new();
        for &x in &nodes {
            let content = self.blame_content(x, p, redacted);
            let count = diff::split_lines(&content).len();
            let mut origins: Vec<Option<(usize, u32)>> = vec![None; count];
            for pn in self.node_parents(x, p) {
                let parent_content = self.blame_content(pn, p, redacted);
                let parent_lines = memo.get(&pn).cloned().unwrap_or_default();
                for (j, m) in diff::line_origins(&parent_content, &content)
                    .into_iter()
                    .enumerate()
                {
                    if let (Some(i), Some(slot)) = (m, origins.get_mut(j))
                        && slot.is_none()
                    {
                        *slot = parent_lines.get(i).copied();
                    }
                }
            }
            let lines: Vec<(usize, u32)> = origins
                .into_iter()
                .enumerate()
                .map(|(j, o)| o.unwrap_or((x, j as u32)))
                .collect();
            memo.insert(x, lines);
        }
        memo.remove(&n).unwrap_or_default()
    }

    /// Distinct contents ever held at `p` in history reachable from `c`.
    ///
    /// Every commit reachable from `c` (BFS from `c`, parents in order) where `p` is a file
    /// contributes its content, in first-seen order. This includes contents from before a
    /// deletion and re-creation of `p` (SPEC 13.2 "every distinct blob the path ever held").
    pub fn all_versions(&self, c: usize, p: &[u8]) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        if c >= self.commits.len() {
            return out;
        }
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([c]);
        while let Some(x) = queue.pop_front() {
            if !seen.insert(x) {
                continue;
            }
            if let Some(f) = self.tree(x).get(p)
                && !out.contains(&f.content)
            {
                out.push(f.content.clone());
            }
            queue.extend(self.parents(x));
        }
        out
    }
}
