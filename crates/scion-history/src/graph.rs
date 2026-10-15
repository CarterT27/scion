//! Commit-graph queries (SPEC 9.2, 18.5; P13).
//!
//! Every query reads the SPEC 9.2 `commit_graph` columns (`generation`, `skip`) from the
//! `changesets` rows and the parents from the decoded changeset, both through
//! [`scion_store::ObjectRepo::get_changeset`] (decoded LRU; stale read first, strong retry on a
//! miss, SPEC 5.4). Within one query, nodes are memoized in a per-call [`Walker`].
//!
//! Ordering used by the walks: the key of a changeset is `(generation, id)`; "newest first" means
//! descending key. A parent always has a strictly smaller generation than its child, so a
//! max-heap over this key visits every node after all of its descendants that are reachable from
//! the walk's starting points.

use crate::history::History;
use futures::future::try_join_all;
use scion_core::{ErrorCode, ObjectId, Result, ScionError};
use scion_object::enc::{Decoder, Encoder};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

pub(crate) use scion_core::subcode::BAD_PAGE_TOKEN;

/// Format version of the opaque `log_page` token.
const PAGE_TOKEN_VERSION: u64 = 1;

/// Flags of the merge-base paint walk.
const FROM_A: u8 = 1;
const FROM_B: u8 = 2;
const STALE: u8 = 4;

/// The commit-graph view of one changeset (SPEC 9.2).
#[derive(Debug)]
struct GraphNode {
    generation: u64,
    parents: Vec<ObjectId>,
    skip: Vec<ObjectId>,
    fp_merges: Option<u64>,
}

/// Per-query memo over [`scion_store::ObjectRepo::get_changeset`].
struct Walker<'a> {
    history: &'a History,
    memo: HashMap<ObjectId, Arc<GraphNode>>,
}

impl<'a> Walker<'a> {
    fn new(history: &'a History) -> Self {
        Walker {
            history,
            memo: HashMap::new(),
        }
    }

    /// The node of `id`; NOT_FOUND when the changeset does not exist, INVALID_ARGUMENT when `id`
    /// is not a changeset id.
    async fn node(&mut self, id: &ObjectId) -> Result<Arc<GraphNode>> {
        if let Some(n) = self.memo.get(id) {
            return Ok(n.clone());
        }
        let (cs, row) = self.history.objects.get_changeset(id).await?;
        let n = Arc::new(GraphNode {
            generation: row.generation,
            parents: cs.parents.clone(),
            skip: row.skip,
            fp_merges: row.fp_merges,
        });
        self.memo.insert(*id, n.clone());
        Ok(n)
    }

    /// Load every id not yet memoized concurrently (one round trip per id on remote stores,
    /// overlapped).
    async fn prefetch(&mut self, ids: &[ObjectId]) -> Result<()> {
        let missing: Vec<ObjectId> = ids
            .iter()
            .filter(|id| !self.memo.contains_key(id))
            .copied()
            .collect();
        if missing.len() < 2 {
            return Ok(());
        }
        let objects = &self.history.objects;
        let loaded = try_join_all(missing.iter().map(|id| objects.get_changeset(id))).await?;
        for (id, (cs, row)) in missing.into_iter().zip(loaded) {
            self.memo.insert(
                id,
                Arc::new(GraphNode {
                    generation: row.generation,
                    parents: cs.parents.clone(),
                    skip: row.skip,
                    fp_merges: row.fp_merges,
                }),
            );
        }
        Ok(())
    }

    /// The deepest first-parent ancestor `y` of `id` (inclusive) with generation
    /// `>= min_gen`, when the chain from `id` down to `y` (both included) holds no merge. The
    /// chain nodes are then the only ancestors of `id` with generation `>= min_gen`, and `y`'s
    /// parent (at most one) lies below `min_gen`. `None` when the span holds a merge, a
    /// `fp_merges` count is unknown, or `generation(id) < min_gen`.
    ///
    /// Follows the skip pointers like [`History::is_first_parent_ancestor`]: O(log n) reads.
    /// `fp_merges` never increases going down a first-parent chain, so the first jump target
    /// whose count differs from `id`'s already proves a merge.
    async fn merge_free_span(
        &mut self,
        id: &ObjectId,
        n: &Arc<GraphNode>,
        min_gen: u64,
    ) -> Result<Option<(ObjectId, Arc<GraphNode>)>> {
        let Some(top) = n.fp_merges else {
            return Ok(None);
        };
        if n.generation < min_gen {
            return Ok(None);
        }
        let (mut c, mut cn) = (*id, n.clone());
        let mut limit = usize::MAX;
        loop {
            let mut next = None;
            for (k, s) in cn.skip.iter().enumerate().take(limit).rev() {
                let sn = self.node(s).await?;
                if sn.generation >= min_gen {
                    next = Some((k, *s, sn));
                    break;
                }
            }
            let Some((k, s, sn)) = next else { break };
            if sn.fp_merges != Some(top) {
                return Ok(None);
            }
            (limit, c, cn) = (k, s, sn);
        }
        Ok((cn.fp_merges == Some(top) && cn.parents.len() <= 1).then_some((c, cn)))
    }
}

fn bad_token(message: impl Into<String>) -> ScionError {
    ScionError::invalid(BAD_PAGE_TOKEN, message)
}

/// Encode a `log_page` continuation token: `uvarint version, id from, u8 first_parent_only,
/// list<id> frontier` (frontier ascending by id, so equal states give equal tokens).
fn encode_page_token(from: &ObjectId, first_parent_only: bool, frontier: &[ObjectId]) -> Vec<u8> {
    let mut sorted = frontier.to_vec();
    sorted.sort();
    let mut e = Encoder::new();
    e.uvarint(PAGE_TOKEN_VERSION);
    e.id(from);
    e.u8(u8::from(first_parent_only));
    e.uvarint(sorted.len() as u64);
    for id in &sorted {
        e.id(id);
    }
    e.finish()
}

/// Decode and validate a token produced by [`encode_page_token`] for the same query.
fn decode_page_token(
    token: &[u8],
    from: &ObjectId,
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    let parse = || -> Result<(ObjectId, u8, Vec<ObjectId>)> {
        let mut d = Decoder::new(token);
        if d.uvarint()? != PAGE_TOKEN_VERSION {
            return Err(bad_token("unknown page token version"));
        }
        let tf = d.id()?;
        let flag = d.u8()?;
        let n = d.list_len()?;
        let mut frontier = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            frontier.push(d.id()?);
        }
        d.finish()?;
        Ok((tf, flag, frontier))
    };
    let (tf, flag, frontier) = parse().map_err(|e| {
        if e.subcode() == Some(BAD_PAGE_TOKEN) {
            e
        } else {
            bad_token(format!("malformed page token: {}", e.message))
        }
    })?;
    if tf != *from {
        return Err(bad_token(
            "page token belongs to a different starting changeset",
        ));
    }
    if flag > 1 || (flag == 1) != first_parent_only {
        return Err(bad_token(
            "page token belongs to a different first_parent_only mode",
        ));
    }
    if frontier.is_empty() {
        return Err(bad_token("page token has an empty frontier"));
    }
    if !frontier.windows(2).all(|w| w[0] < w[1]) {
        return Err(bad_token("page token frontier is not strictly ascending"));
    }
    if frontier
        .iter()
        .any(|id| id.kind() != scion_core::Kind::Changeset)
    {
        return Err(bad_token("page token frontier holds a non-changeset id"));
    }
    Ok(frontier)
}

impl History {
    /// SPEC 18.5 Is Ancestor: is `a` an ancestor of (or equal to) `b`?
    ///
    /// Generation-pruned depth-first search from `b` over all parents (SPEC 9.2 `commit_graph`):
    /// a changeset whose generation is `<= generation(a)` cannot have `a` as a proper ancestor,
    /// so the search does not expand it. `a` is an ancestor of itself. A merge-free
    /// first-parent span (SPEC 9.2 `fp_merges`) is crossed with the skip pointers, so a linear
    /// gap costs O(log n) reads.
    ///
    /// Errors: NOT_FOUND when `a` or `b` (or a stored parent) is missing; INVALID_ARGUMENT /
    /// `invalid_kind` for a non-changeset id.
    pub async fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        let mut w = Walker::new(self);
        let ga = w.node(a).await?.generation;
        let gb = w.node(b).await?.generation;
        if a == b {
            return Ok(true);
        }
        if gb <= ga {
            return Ok(false);
        }
        let mut stack = vec![*b];
        let mut seen: HashSet<ObjectId> = HashSet::new();
        while let Some(c) = stack.pop() {
            if c == *a {
                return Ok(true);
            }
            if !seen.insert(c) {
                continue;
            }
            let n = w.node(&c).await?;
            if n.generation <= ga {
                continue;
            }
            // A merge-free first-parent span holds every ancestor of `c` down to
            // generation(a): jump over it (O(log n) reads) instead of walking it.
            if let Some((y, _)) = w.merge_free_span(&c, &n, ga).await? {
                if y == *a {
                    return Ok(true);
                }
                continue;
            }
            w.prefetch(&n.parents).await?;
            // Reverse push so the first parent is explored first (the common linear case).
            for p in n.parents.iter().rev() {
                if !seen.contains(p) {
                    stack.push(*p);
                }
            }
        }
        Ok(false)
    }

    /// SPEC 18.5 `is_first_parent_ancestor`: is `a` on the first-parent chain of `b` (or equal
    /// to `b`)?
    ///
    /// Uses the SPEC 9.2 skip pointers: repeatedly jump to the farthest `skip[k]` whose
    /// generation is still `>= generation(a)`. Generations strictly decrease along a
    /// first-parent chain, so the chain nodes with generation `>= generation(a)` form a prefix
    /// of length `d`, and each jump takes the largest `2^k <= d` (binary decomposition of `d`).
    /// After a jump by `2^k` the remaining distance is below `2^k`, so the next jump only needs
    /// to consider pointers below `k`: O(log n) changeset reads in total.
    pub async fn is_first_parent_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        let mut w = Walker::new(self);
        let ga = w.node(a).await?.generation;
        let mut c = *b;
        let mut cn = w.node(b).await?;
        // Exclusive upper bound on the skip index that can still be useful.
        let mut limit = usize::MAX;
        loop {
            if cn.generation <= ga {
                return Ok(c == *a);
            }
            let mut next = None;
            for (k, s) in cn.skip.iter().enumerate().take(limit).rev() {
                let sn = w.node(s).await?;
                if sn.generation >= ga {
                    next = Some((k, *s, sn));
                    break;
                }
            }
            match next {
                Some((k, s, sn)) => {
                    limit = k;
                    c = s;
                    cn = sn;
                }
                // Root reached, or the first parent is already below generation(a).
                None => return Ok(false),
            }
        }
    }

    /// All best common ancestors of `a` and `b`, ascending by id (SPEC 7.4 `MergeBase`, 8.3).
    ///
    /// A best common ancestor is a common ancestor (inclusive: `a` itself when `a` is an
    /// ancestor of `b`) that is not a proper ancestor of another common ancestor. Computed by a
    /// generation-ordered paint walk: nodes are visited newest first by `(generation, id)`, so
    /// each node's reachability flags are final when it is visited; a node reachable from both
    /// sides and not below another common ancestor is a result, and everything below it is
    /// marked stale. The walk stops as soon as only stale nodes remain queued. A merge-free
    /// first-parent span above every other queued node is crossed with the skip pointers
    /// (SPEC 9.2 `fp_merges`), so a long linear side costs O(log n) reads. Returns an empty
    /// list for unrelated histories (e.g. two imported roots); a criss-cross yields two bases.
    pub async fn merge_bases(&self, a: &ObjectId, b: &ObjectId) -> Result<Vec<ObjectId>> {
        let mut w = Walker::new(self);
        let na = w.node(a).await?;
        let nb = w.node(b).await?;
        if a == b {
            return Ok(vec![*a]);
        }
        let mut flags: HashMap<ObjectId, u8> = HashMap::new();
        let mut heap: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
        flags.insert(*a, FROM_A);
        flags.insert(*b, FROM_B);
        heap.push((na.generation, *a));
        heap.push((nb.generation, *b));
        // Number of queued nodes without STALE.
        let mut live = 2usize;
        let mut result = Vec::new();
        while live > 0 {
            let Some((_, id)) = heap.pop() else { break };
            let f = flags.get(&id).copied().unwrap_or(0);
            let mut down = f;
            if f & STALE == 0 {
                live -= 1;
                if f & (FROM_A | FROM_B) == FROM_A | FROM_B {
                    result.push(id);
                    down |= STALE;
                }
            }
            let n = w.node(&id).await?;
            w.prefetch(&n.parents).await?;
            let mut paint = n.parents.clone();
            // Every other queued node is older than `floor`, so the first-parent chain below
            // `n` down to `floor` is reachable from `n` only: when it holds no merge, it would
            // only be painted with `down` and never become a result. Paint below it instead.
            if let [p] = n.parents.as_slice() {
                let floor = heap.peek().map_or(0, |(g, _)| *g) + 1;
                let pn = w.node(p).await?;
                if let Some((_, y)) = w.merge_free_span(p, &pn, floor).await? {
                    paint = y.parents.clone();
                }
            }
            for p in &paint {
                let old = flags.get(p).copied();
                let new = old.unwrap_or(0) | down;
                if old == Some(new) {
                    continue;
                }
                flags.insert(*p, new);
                match old {
                    None => {
                        let g = w.node(p).await?.generation;
                        heap.push((g, *p));
                        if new & STALE == 0 {
                            live += 1;
                        }
                    }
                    Some(o) => {
                        // Still queued: a parent's key is below every node visited so far.
                        if o & STALE == 0 && new & STALE != 0 {
                            live = live.saturating_sub(1);
                        }
                    }
                }
            }
        }
        result.sort();
        Ok(result)
    }

    /// `c~n` (SPEC 11.8): the n-th first-parent ancestor; `c~0 == c`; `None` beyond the root.
    ///
    /// Decomposes `n` into powers of two and follows the SPEC 9.2 skip pointers, so it reads
    /// O(log n) changesets.
    pub async fn nth_first_parent(&self, c: &ObjectId, n: u32) -> Result<Option<ObjectId>> {
        let mut w = Walker::new(self);
        let mut cur = *c;
        let mut cn = w.node(c).await?;
        let mut rem = n;
        while rem > 0 {
            let k = (u32::BITS - 1 - rem.leading_zeros()) as usize;
            match cn.skip.get(k) {
                Some(s) => {
                    cur = *s;
                    cn = w.node(s).await?;
                    rem -= 1u32 << k;
                }
                // The chain below `cur` is shorter than 2^k <= rem.
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// `c^k` (SPEC 11.8): the k-th parent (1-based); `None` when `c` has fewer than `k` parents.
    ///
    /// `k == 0` is INVALID_ARGUMENT / `invalid_revision` (`^0` is invalid,
    /// implementation-defined 6.6); NOT_FOUND when `c` is missing.
    pub async fn nth_parent(&self, c: &ObjectId, k: u32) -> Result<Option<ObjectId>> {
        let mut w = Walker::new(self);
        let n = w.node(c).await?;
        let Some(i) = (k as usize).checked_sub(1) else {
            return Err(ScionError::invalid(
                scion_core::subcode::INVALID_REVISION,
                "^0 is invalid",
            ));
        };
        Ok(n.parents.get(i).copied())
    }

    /// One page of history from `from` (SPEC 7.4 `Log` without a path), newest first: descending
    /// generation, ties by descending id. `first_parent_only` follows only `parents[0]`.
    ///
    /// `after` is the opaque page token returned by the previous page of the same query; it
    /// encodes the walk frontier, so paging is stable (concatenated pages equal the full walk)
    /// and needs no server-side state. The returned token is `None` when the walk is exhausted.
    /// A `limit` of 0 returns an empty page and a token for the unchanged position.
    ///
    /// Errors: INVALID_ARGUMENT / `bad_page_token` for a malformed token or one issued for a
    /// different `from` or `first_parent_only`; NOT_FOUND when `from` is missing.
    pub async fn log_page(
        &self,
        from: &ObjectId,
        first_parent_only: bool,
        limit: usize,
        after: Option<&[u8]>,
    ) -> Result<(Vec<ObjectId>, Option<Vec<u8>>)> {
        let mut w = Walker::new(self);
        let start = match after {
            None => vec![*from],
            Some(token) => {
                // `from` must exist even when continuing.
                w.node(from).await?;
                decode_page_token(token, from, first_parent_only)?
            }
        };
        let mut heap: BinaryHeap<(u64, ObjectId)> = BinaryHeap::new();
        let mut queued: HashSet<ObjectId> = HashSet::new();
        w.prefetch(&start).await?;
        for id in start {
            let g = match w.node(&id).await {
                Ok(n) => n.generation,
                Err(e) if after.is_some() && e.code == ErrorCode::NotFound => {
                    return Err(bad_token("page token names an unknown changeset"));
                }
                Err(e) => return Err(e),
            };
            queued.insert(id);
            heap.push((g, id));
        }
        // Invariant: `heap` holds exactly the not-yet-emitted nodes with an emitted child (or
        // the start). A parent's key is below its child's, so it is never pushed after being
        // emitted, and membership in `queued` is the only deduplication needed.
        let mut out = Vec::with_capacity(limit.min(1024));
        while out.len() < limit {
            let Some((_, id)) = heap.pop() else { break };
            queued.remove(&id);
            out.push(id);
            let n = w.node(&id).await?;
            let parents: &[ObjectId] = if first_parent_only {
                n.parents.get(..1).unwrap_or(&[])
            } else {
                &n.parents
            };
            w.prefetch(parents).await?;
            for p in parents {
                if queued.insert(*p) {
                    let g = w.node(p).await?.generation;
                    heap.push((g, *p));
                }
            }
        }
        let next = if heap.is_empty() {
            None
        } else {
            let frontier: Vec<ObjectId> = heap.iter().map(|(_, id)| *id).collect();
            Some(encode_page_token(from, first_parent_only, &frontier))
        };
        Ok((out, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    fn id(n: u8) -> ObjectId {
        ObjectId::new(Kind::Changeset, [n; 32])
    }

    #[test]
    fn page_token_round_trip_and_validation() {
        let from = id(9);
        let t = encode_page_token(&from, true, &[id(3), id(1), id(2)]);
        assert_eq!(
            decode_page_token(&t, &from, true).unwrap(),
            vec![id(1), id(2), id(3)]
        );
        // Mismatched query.
        let e = decode_page_token(&t, &id(8), true).unwrap_err();
        assert_eq!(e.subcode(), Some(BAD_PAGE_TOKEN));
        let e = decode_page_token(&t, &from, false).unwrap_err();
        assert_eq!(e.subcode(), Some(BAD_PAGE_TOKEN));
        // Garbage, truncation and trailing bytes.
        for bad in [
            vec![],
            vec![0xff],
            t[..t.len() - 1].to_vec(),
            [t.clone(), vec![0]].concat(),
        ] {
            let e = decode_page_token(&bad, &from, true).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert_eq!(e.subcode(), Some(BAD_PAGE_TOKEN));
        }
        // Empty frontier and non-changeset ids are rejected.
        let empty = encode_page_token(&from, false, &[]);
        assert!(decode_page_token(&empty, &from, false).is_err());
        let tree = ObjectId::new(Kind::Tree, [1; 32]);
        let t2 = encode_page_token(&from, false, &[tree]);
        assert!(decode_page_token(&t2, &from, false).is_err());
        // Unsorted / duplicate frontier (hand-built).
        let mut e = Encoder::new();
        e.uvarint(PAGE_TOKEN_VERSION);
        e.id(&from);
        e.u8(0);
        e.uvarint(2);
        e.id(&id(2));
        e.id(&id(2));
        assert!(decode_page_token(&e.finish(), &from, false).is_err());
    }
}
