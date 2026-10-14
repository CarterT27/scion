//! Seeded corpus generator (SPEC 19.0 Corpus; P09).
//!
//! [`generate`] simulates a small team: branches fork from earlier commits, receive line-level
//! text edits (so three-way merges are meaningful), file additions, deletions, mode flips,
//! symlink retargets and binary rewrites, and are joined by two-parent merges, octopus merges
//! and criss-cross merge pairs. Everything is driven by one `rand_chacha` stream seeded from
//! [`CorpusSpec::seed`], and all collections are ordered, so the output is a pure function of
//! the spec.
//!
//! Merge snapshots are computed by a simple file-level three-way rule against the highest
//! common ancestor: a path changed on one side only takes that side; a path changed on both
//! sides keeps the first parent's version (like resolving every conflict with "ours"). The
//! snapshot is what a merge commit records; consumers recompute merges with their own engines.
//!
//! Memory: every commit carries a full [`scion_refmodel::MTree`], so a repository costs
//! O(history_len x files) entries. Keep `files x history_len` in the low millions.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_refmodel::{MFile, MTree};
use std::collections::BTreeSet;

/// Shape of a generated repository (SPEC 19.0 Corpus: file count, depth, fan-out, history
/// length, plus merge and content mix).
#[derive(Clone, Debug)]
pub struct CorpusSpec {
    /// Files in the final tree (approximate).
    pub files: u64,
    /// Maximum path depth in components (files sit in directories of at most `depth - 1`
    /// components).
    pub depth: u32,
    /// Entries per directory: every directory has at most `fanout` subdirectories; files are
    /// spread uniformly over all directories.
    pub fanout: u32,
    /// Number of commits.
    pub history_len: u32,
    /// Fraction of two-parent merges.
    pub merge_ratio: f64,
    /// Fraction of octopus merges.
    pub octopus_ratio: f64,
    /// Fraction of binary files.
    pub binary_ratio: f64,
    /// Fraction of symlinks.
    pub symlink_ratio: f64,
    /// RNG seed (output is deterministic per seed).
    pub seed: u64,
}

impl Default for CorpusSpec {
    /// A small, merge-rich default: 200 files, depth 4, fanout 6, 60 commits.
    fn default() -> Self {
        CorpusSpec {
            files: 200,
            depth: 4,
            fanout: 6,
            history_len: 60,
            merge_ratio: 0.15,
            octopus_ratio: 0.05,
            binary_ratio: 0.05,
            symlink_ratio: 0.05,
            seed: 0,
        }
    }
}

/// One generated commit: parents by index plus a full structural snapshot (SPEC 19.0).
#[derive(Clone, Debug)]
pub struct GenCommit {
    /// Parent indices (earlier commits).
    pub parents: Vec<usize>,
    /// Full file-level snapshot.
    pub tree: scion_refmodel::MTree,
    /// Author bytes.
    pub author: Vec<u8>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Change id.
    pub change_id: [u8; 16],
}

/// A generated history in topological order (SPEC 19.0 Corpus).
#[derive(Clone, Debug)]
pub struct GenRepo {
    /// Commits, parents first.
    pub commits: Vec<GenCommit>,
}

/// Generate a repository from `spec` (SPEC 19.0 Corpus; rand_chacha, deterministic).
///
/// - Commit 0 is the only root; it holds exactly `spec.files` files. Every path has at most
///   `spec.depth` components; each directory has at most `spec.fanout` subdirectories, and a
///   new file lands in a directory chosen uniformly among all of them.
/// - The history has `max(spec.history_len, 1)` commits in topological order.
/// - Each later commit is an octopus merge (probability `octopus_ratio`, 3 to 5 parents), a
///   two-parent merge (`merge_ratio`; about a quarter of these are emitted as a criss-cross
///   pair of merges), or a regular commit on a branch. Missing branches are forked first.
/// - New files are binary with probability `binary_ratio` (they contain NUL bytes), symlinks
///   with `symlink_ratio`, and text otherwise (some executable).
///
/// Ratios outside `[0, 1]` (or NaN) are clamped.
pub fn generate(spec: &CorpusSpec) -> GenRepo {
    let mut g = Gen::new(spec);
    let tree = g.populate(spec.files);
    g.push(Vec::new(), tree, "root".to_string());
    g.heads.push(0);
    let target = spec.history_len.max(1) as usize;
    while g.commits.len() < target {
        let remaining = target - g.commits.len();
        let roll: f64 = g.rng.random();
        if roll < g.octopus_ratio {
            g.step_octopus();
        } else if roll < g.octopus_ratio + g.merge_ratio {
            g.step_merge(remaining);
        } else {
            g.step_regular();
        }
    }
    g.commits.truncate(target);
    GenRepo { commits: g.commits }
}

impl GenRepo {
    /// A canonical criss-cross history (SPEC 19.0 Corpus, 19.4 ambiguous merge bases):
    /// root `0`; `a1` and `b1` edit it; `m1 = merge(a1, b1)` and `m2 = merge(b1, a1)`; `a2` and
    /// `b2` edit `m1` and `m2`. The heads `a2` and `b2` have two merge bases, `a1` and `b1`.
    /// Contents follow `spec` (file count, mix); history-shape fields are ignored.
    pub fn criss_cross(spec: &CorpusSpec) -> GenRepo {
        let mut g = Gen::new(spec);
        let tree = g.populate(spec.files);
        g.push(Vec::new(), tree, "root".to_string());
        let a1 = g.edit_commit(0, "a1");
        let b1 = g.edit_commit(0, "b1");
        let m1 = g.merge_commit(&[a1, b1], "m1: merge b1 into a1");
        let m2 = g.merge_commit(&[b1, a1], "m2: merge a1 into b1");
        g.edit_commit(m1, "a2");
        g.edit_commit(m2, "b2");
        GenRepo { commits: g.commits }
    }

    /// A history with `roots` independent root commits (at least 2), each followed by a few
    /// edits, joined by one merge whose parents are the branch tips (an octopus when `roots`
    /// is 3 or more). Models imported unrelated histories (SPEC 14, 19.14). Root `N` starts
    /// with its files under the top-level directory `rootN`; later edits may add files
    /// elsewhere, but file names are unique across the whole history, so the joined trees
    /// never collide.
    pub fn multi_root(spec: &CorpusSpec, roots: usize) -> GenRepo {
        let roots = roots.max(2);
        let mut g = Gen::new(spec);
        let per_root = (spec.files / roots as u64).max(1);
        let mut tips = Vec::new();
        for r in 0..roots {
            let base = g.populate(per_root);
            let tree: MTree = base
                .into_iter()
                .map(|(p, f)| {
                    let mut np = format!("root{r}/").into_bytes();
                    np.extend_from_slice(&p);
                    (np, f)
                })
                .collect();
            let root = g.push(Vec::new(), tree, format!("root {r}"));
            let e1 = g.edit_commit(root, &format!("root {r} edit 1"));
            let e2 = g.edit_commit(e1, &format!("root {r} edit 2"));
            tips.push(e2);
        }
        g.merge_commit(&tips, "join unrelated histories");
        GenRepo { commits: g.commits }
    }

    /// Indices of parentless commits.
    pub fn roots(&self) -> Vec<usize> {
        (0..self.commits.len())
            .filter(|i| self.commits[*i].parents.is_empty())
            .collect()
    }

    /// True when `a` is an ancestor of `b` (reflexive).
    pub fn is_ancestor(&self, a: usize, b: usize) -> bool {
        ancestors(&self.commits, b).get(a).copied().unwrap_or(false)
    }

    /// The maximal common ancestors of `a` and `b` (all merge bases), ascending. More than one
    /// means a criss-cross shape.
    pub fn merge_bases(&self, a: usize, b: usize) -> Vec<usize> {
        let aa = ancestors(&self.commits, a);
        let ab = ancestors(&self.commits, b);
        let common: Vec<usize> = (0..self.commits.len())
            .filter(|i| {
                aa.get(*i).copied().unwrap_or(false) && ab.get(*i).copied().unwrap_or(false)
            })
            .collect();
        let mut out = Vec::new();
        for &c in &common {
            let dominated = common
                .iter()
                .any(|&d| d != c && ancestors(&self.commits, d).get(c).copied().unwrap_or(false));
            if !dominated {
                out.push(c);
            }
        }
        out
    }
}

/// Reachability from `c` (inclusive) as a bitmap over commit indices.
fn ancestors(commits: &[GenCommit], c: usize) -> Vec<bool> {
    let mut seen = vec![false; commits.len()];
    let mut stack = vec![c];
    while let Some(x) = stack.pop() {
        match seen.get_mut(x) {
            Some(s) if !*s => *s = true,
            _ => continue,
        }
        if let Some(commit) = commits.get(x) {
            stack.extend(commit.parents.iter().copied());
        }
    }
    seen
}

const MAX_HEADS: usize = 8;

const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon", "phi", "chi",
    "psi", "omega",
];

const AUTHORS: &[&str] = &[
    "Ada Lovelace <ada@example.com>",
    "Grace Hopper <grace@example.com>",
    "Alan Turing <alan@example.com>",
    "Edsger Dijkstra <edsger@example.com>",
    "Barbara Liskov <barbara@example.com>",
];

fn clamp_ratio(r: f64) -> f64 {
    if r.is_nan() { 0.0 } else { r.clamp(0.0, 1.0) }
}

struct Gen {
    rng: ChaCha8Rng,
    depth: u32,
    fanout: u32,
    binary_ratio: f64,
    symlink_ratio: f64,
    merge_ratio: f64,
    octopus_ratio: f64,
    commits: Vec<GenCommit>,
    heads: Vec<usize>,
    next_file: u64,
}

impl Gen {
    fn new(spec: &CorpusSpec) -> Self {
        Gen {
            rng: ChaCha8Rng::seed_from_u64(spec.seed),
            depth: spec.depth.max(1),
            fanout: spec.fanout.max(1),
            binary_ratio: clamp_ratio(spec.binary_ratio),
            symlink_ratio: clamp_ratio(spec.symlink_ratio),
            merge_ratio: clamp_ratio(spec.merge_ratio),
            octopus_ratio: clamp_ratio(spec.octopus_ratio),
            commits: Vec::new(),
            heads: Vec::new(),
            next_file: 0,
        }
    }

    fn push(&mut self, parents: Vec<usize>, tree: MTree, what: String) -> usize {
        let idx = self.commits.len();
        let author = AUTHORS[self.rng.random_range(0..AUTHORS.len())]
            .as_bytes()
            .to_vec();
        let mut change_id = [0u8; 16];
        self.rng.fill(&mut change_id);
        let message = format!("gen {idx}: {what}\n").into_bytes();
        self.commits.push(GenCommit {
            parents,
            tree,
            author,
            message,
            change_id,
        });
        idx
    }

    // ---- content ----

    fn word(&mut self) -> &'static str {
        WORDS[self.rng.random_range(0..WORDS.len())]
    }

    fn text_line(&mut self, tag: &str) -> Vec<u8> {
        let a = self.word();
        let b = self.word();
        let n: u32 = self.rng.random_range(0..10_000);
        format!("{tag} {a} {b} {n}\n").into_bytes()
    }

    fn text_content(&mut self) -> Vec<u8> {
        let lines = self.rng.random_range(3..30);
        let mut out = Vec::new();
        for i in 0..lines {
            out.extend(self.text_line(&format!("line{i}")));
        }
        out
    }

    fn binary_content(&mut self) -> Vec<u8> {
        let len = self.rng.random_range(16..256);
        let mut out = vec![0u8; len];
        self.rng.fill(out.as_mut_slice());
        let nul = self.rng.random_range(0..len);
        out[nul] = 0;
        out
    }

    /// A directory chosen uniformly among the `sum(fanout^l)` possible directories of at most
    /// `depth - 1` components, so files spread evenly over directories instead of piling up
    /// near the root (a uniform choice of level would put `1/depth` of all files in the root).
    fn random_dir(&mut self) -> Vec<u8> {
        let mut weights: Vec<u64> = Vec::with_capacity(self.depth as usize);
        let mut w: u64 = 1;
        for _ in 0..self.depth {
            weights.push(w);
            w = w.saturating_mul(u64::from(self.fanout));
        }
        let total = weights.iter().fold(0u64, |a, b| a.saturating_add(*b));
        let mut pick = self.rng.random_range(0..total.max(1));
        let mut levels = 0u32;
        for (l, w) in weights.iter().enumerate() {
            if pick < *w {
                levels = u32::try_from(l).unwrap_or(0);
                break;
            }
            pick -= *w;
        }
        let mut dir = Vec::new();
        for _ in 0..levels {
            let d = self.rng.random_range(0..self.fanout);
            dir.extend_from_slice(format!("d{d}/").as_bytes());
        }
        dir
    }

    /// A new file at a fresh path. File names never start with `d`, so they never collide
    /// with directory names.
    fn new_file(&mut self) -> (Vec<u8>, MFile) {
        let n = self.next_file;
        self.next_file += 1;
        let mut path = self.random_dir();
        let roll: f64 = self.rng.random();
        let (name, file) = if roll < self.binary_ratio {
            let content = self.binary_content();
            (format!("b{n}.bin"), MFile { mode: 1, content })
        } else if roll < self.binary_ratio + self.symlink_ratio {
            let t: u32 = self.rng.random_range(0..1000);
            let content = format!("../target-{t}").into_bytes();
            (format!("l{n}"), MFile { mode: 3, content })
        } else if self.rng.random_bool(0.05) {
            let mut content = b"#!/bin/sh\n".to_vec();
            content.extend(self.text_content());
            (format!("x{n}.sh"), MFile { mode: 2, content })
        } else {
            let content = self.text_content();
            (format!("f{n}.txt"), MFile { mode: 1, content })
        };
        path.extend_from_slice(name.as_bytes());
        (path, file)
    }

    fn populate(&mut self, files: u64) -> MTree {
        let mut t = MTree::new();
        for _ in 0..files {
            let (p, f) = self.new_file();
            t.insert(p, f);
        }
        t
    }

    // ---- edits ----

    fn pick<'a>(&mut self, keys: &'a [Vec<u8>]) -> Option<&'a Vec<u8>> {
        if keys.is_empty() {
            None
        } else {
            keys.get(self.rng.random_range(0..keys.len()))
        }
    }

    /// Line-level edits of one text file: replace, insert or delete 1-3 lines.
    fn edit_lines(&mut self, content: &[u8], tag: &str) -> Vec<u8> {
        let mut lines: Vec<Vec<u8>> = content
            .split_inclusive(|b| *b == b'\n')
            .map(|l| l.to_vec())
            .collect();
        let ops = self.rng.random_range(1..=3);
        for k in 0..ops {
            let line = self.text_line(&format!("{tag}.{k}"));
            match self.rng.random_range(0..3) {
                0 if !lines.is_empty() => {
                    let i = self.rng.random_range(0..lines.len());
                    lines[i] = line;
                }
                1 if lines.len() > 1 => {
                    let i = self.rng.random_range(0..lines.len());
                    lines.remove(i);
                }
                _ => {
                    let i = self.rng.random_range(0..=lines.len());
                    lines.insert(i, line);
                }
            }
        }
        lines.concat()
    }

    /// Apply 1-3 random edits to `tree`; guarantees the tree changes.
    fn mutate(&mut self, tree: &mut MTree, tag: &str) {
        let before = tree.clone();
        let ops = self.rng.random_range(1..=3);
        for _ in 0..ops {
            let roll: f64 = self.rng.random();
            let text: Vec<Vec<u8>> = tree
                .iter()
                .filter(|(_, f)| f.mode != 3 && !f.content.contains(&0))
                .map(|(p, _)| p.clone())
                .collect();
            if roll < 0.62 {
                if let Some(p) = self.pick(&text).cloned() {
                    if let Some(f) = tree.get(&p).cloned() {
                        let content = self.edit_lines(&f.content, tag);
                        tree.insert(
                            p,
                            MFile {
                                mode: f.mode,
                                content,
                            },
                        );
                    }
                    continue;
                }
                let (p, f) = self.new_file();
                tree.insert(p, f);
            } else if roll < 0.74 {
                let (p, f) = self.new_file();
                tree.insert(p, f);
            } else if roll < 0.84 {
                if tree.len() > 1 {
                    let keys: Vec<Vec<u8>> = tree.keys().cloned().collect();
                    if let Some(p) = self.pick(&keys).cloned() {
                        tree.remove(&p);
                    }
                }
            } else if roll < 0.90 {
                if let Some(p) = self.pick(&text).cloned()
                    && let Some(f) = tree.get_mut(&p)
                {
                    f.mode = if f.mode == 1 { 2 } else { 1 };
                }
            } else if roll < 0.95 {
                let links: Vec<Vec<u8>> = tree
                    .iter()
                    .filter(|(_, f)| f.mode == 3)
                    .map(|(p, _)| p.clone())
                    .collect();
                if let Some(p) = self.pick(&links).cloned() {
                    let t: u32 = self.rng.random_range(0..1000);
                    tree.insert(
                        p,
                        MFile {
                            mode: 3,
                            content: format!("../retarget-{t}").into_bytes(),
                        },
                    );
                }
            } else {
                let bins: Vec<Vec<u8>> = tree
                    .iter()
                    .filter(|(_, f)| f.mode != 3 && f.content.contains(&0))
                    .map(|(p, _)| p.clone())
                    .collect();
                if let Some(p) = self.pick(&bins).cloned() {
                    let content = self.binary_content();
                    tree.insert(p, MFile { mode: 1, content });
                }
            }
        }
        if *tree == before {
            let (p, f) = self.new_file();
            tree.insert(p, f);
        }
    }

    fn edit_commit(&mut self, parent: usize, what: &str) -> usize {
        let mut tree = self
            .commits
            .get(parent)
            .map(|c| c.tree.clone())
            .unwrap_or_default();
        let tag = format!("c{}", self.commits.len());
        self.mutate(&mut tree, &tag);
        self.push(vec![parent], tree, what.to_string())
    }

    // ---- merges ----

    /// Highest-index common ancestor of `a` and `b` (a maximal merge base).
    fn base_of(&self, a: usize, b: usize) -> Option<usize> {
        let aa = ancestors(&self.commits, a);
        let ab = ancestors(&self.commits, b);
        (0..self.commits.len())
            .rev()
            .find(|i| aa.get(*i).copied().unwrap_or(false) && ab.get(*i).copied().unwrap_or(false))
    }

    /// File-level three-way merge of `parents` into the first parent's tree.
    fn merged_tree(&self, parents: &[usize]) -> MTree {
        let Some(&first) = parents.first() else {
            return MTree::new();
        };
        let mut merged = self
            .commits
            .get(first)
            .map(|c| c.tree.clone())
            .unwrap_or_default();
        let empty = MTree::new();
        for &q in &parents[1..] {
            let Some(theirs) = self.commits.get(q).map(|c| &c.tree) else {
                continue;
            };
            let base = match self.base_of(first, q) {
                Some(b) => self.commits.get(b).map(|c| &c.tree).unwrap_or(&empty),
                None => &empty,
            };
            let paths: BTreeSet<&Vec<u8>> = base.keys().chain(theirs.keys()).collect();
            let mut updates: Vec<(Vec<u8>, Option<MFile>)> = Vec::new();
            for p in paths {
                let b = base.get(p);
                let t = theirs.get(p);
                if b == t {
                    continue;
                }
                // Theirs changed `p`; take it only when ours did not.
                if merged.get(p) == b {
                    updates.push((p.clone(), t.cloned()));
                }
            }
            for (p, f) in updates {
                match f {
                    Some(f) => {
                        merged.insert(p, f);
                    }
                    None => {
                        merged.remove(&p);
                    }
                }
            }
        }
        merged
    }

    fn merge_commit(&mut self, parents: &[usize], what: &str) -> usize {
        let tree = self.merged_tree(parents);
        self.push(parents.to_vec(), tree, what.to_string())
    }

    /// Distinct heads none of which is an ancestor of another, in random order.
    fn independent_heads(&mut self, want: usize) -> Vec<usize> {
        let mut pool = self.heads.clone();
        let mut out: Vec<usize> = Vec::new();
        while !pool.is_empty() && out.len() < want {
            let i = self.rng.random_range(0..pool.len());
            let h = pool.swap_remove(i);
            let related = out.iter().any(|&o| {
                let ho = ancestors(&self.commits, h);
                let oo = ancestors(&self.commits, o);
                ho.get(o).copied().unwrap_or(false) || oo.get(h).copied().unwrap_or(false)
            });
            if !related {
                out.push(h);
            }
        }
        out
    }

    fn replace_head(&mut self, old: usize, new: usize) {
        if let Some(h) = self.heads.iter_mut().find(|h| **h == old) {
            *h = new;
        } else {
            self.heads.push(new);
        }
    }

    fn retire_head(&mut self, h: usize) {
        if self.heads.len() > 1 {
            self.heads.retain(|x| *x != h);
        }
    }

    /// Fork a new branch from a random earlier commit with one regular commit.
    fn step_fork(&mut self) {
        let from = self.rng.random_range(0..self.commits.len());
        let c = self.edit_commit(from, "fork");
        self.heads.push(c);
    }

    fn step_regular(&mut self) {
        let fork_p = if self.heads.len() < 2 { 0.3 } else { 0.1 };
        if self.heads.len() < MAX_HEADS && self.rng.random_bool(fork_p) {
            self.step_fork();
            return;
        }
        let i = self.rng.random_range(0..self.heads.len());
        let h = self.heads[i];
        let c = self.edit_commit(h, "edit");
        self.heads[i] = c;
    }

    fn step_merge(&mut self, remaining: usize) {
        let pair = self.independent_heads(2);
        let [a, b] = pair[..] else {
            if self.heads.len() < MAX_HEADS {
                self.step_fork();
            } else {
                self.step_regular();
            }
            return;
        };
        if remaining >= 2 && self.rng.random_bool(0.25) {
            // Criss-cross: each branch merges the other's current tip.
            let m1 = self.merge_commit(&[a, b], "criss-cross merge");
            let m2 = self.merge_commit(&[b, a], "criss-cross merge");
            self.replace_head(a, m1);
            self.replace_head(b, m2);
            return;
        }
        let m = self.merge_commit(&[a, b], "merge");
        self.replace_head(a, m);
        if self.rng.random_bool(0.5) {
            self.retire_head(b);
        }
    }

    fn step_octopus(&mut self) {
        let want = self.rng.random_range(3..=5);
        let heads = self.independent_heads(want);
        if heads.len() < 3 {
            if self.heads.len() < MAX_HEADS {
                self.step_fork();
            } else {
                self.step_regular();
            }
            return;
        }
        let m = self.merge_commit(&heads, "octopus merge");
        self.replace_head(heads[0], m);
        for &h in &heads[1..] {
            if self.rng.random_bool(0.5) {
                self.retire_head(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_lines_keeps_line_structure() {
        let mut g = Gen::new(&CorpusSpec::default());
        let before = b"a\nb\nc\n".to_vec();
        let after = g.edit_lines(&before, "t");
        assert_ne!(before, after);
        assert!(after.ends_with(b"\n"));
    }

    #[test]
    fn merged_tree_takes_one_sided_changes() {
        let mut g = Gen::new(&CorpusSpec {
            files: 10,
            ..CorpusSpec::default()
        });
        let t = g.populate(10);
        g.push(Vec::new(), t, "root".into());
        let a = g.edit_commit(0, "a");
        let b = g.edit_commit(0, "b");
        let m = g.merge_commit(&[a, b], "m");
        let base = &g.commits[0].tree;
        let ta = &g.commits[a].tree;
        let tb = &g.commits[b].tree;
        let tm = &g.commits[m].tree;
        for (p, f) in tb {
            if base.get(p) != Some(f) && ta.get(p) == base.get(p) {
                assert_eq!(tm.get(p), Some(f));
            }
        }
        for (p, f) in ta {
            assert!(tm.get(p) == Some(f) || base.get(p) == Some(f));
        }
    }
}
