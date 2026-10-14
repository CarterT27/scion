//! Log(path) over history nodes (SPEC 7.4, 9.2; P24).
//!
//! The history of a path is the graph of its history nodes (SPEC 9.2): node reuse makes a
//! changeset that leaves the path's content equal to its first parent's invisible, which is
//! git's TREESAME simplification. [`log_path`] starts at the node of the path in `from` and
//! walks node parents (only the first parent node with `first_parent_only`), emitting each
//! node's `changeset`.
//!
//! Order: descending generation of the node's changeset, ties by descending changeset id (the
//! same order as `History::log_page`). A node's parents belong to proper ancestors, whose
//! generation is strictly lower, so a max-heap walk emits the nodes in that order lazily and a
//! node can never be queued again after it was emitted.
//!
//! Paging: the page token encodes the walk frontier (the queued nodes), so concatenated pages
//! equal the full walk and no server-side state is kept. The token is bound to `from`, the path
//! and `first_parent_only`, and every frontier node is checked to be a node of the path.

use crate::engine::DerivedEngine;
use crate::history::{self, NODE_KIND};
use crate::types::DeriveCtx;
use scion_core::subcode::BAD_PAGE_TOKEN;
use scion_core::{ErrorCode, ObjectId, Result, ScionError};
use scion_object::enc::{Decoder, Encoder};
use scion_object::{HistoryNode, RepoPath};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

/// Format version of the opaque `log_path` page token.
const PAGE_TOKEN_VERSION: u64 = 1;

/// One page of a path log (SPEC 7.4 Log with a path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLogPage {
    /// Changesets that changed the path, newest first.
    pub changesets: Vec<ObjectId>,
    /// Opaque token for the next page.
    pub next_page: Option<Vec<u8>>,
}

fn bad_token(message: impl Into<String>) -> ScionError {
    ScionError::invalid(BAD_PAGE_TOKEN, message)
}

/// `uvarint version, id from, bytes path, u8 first_parent_only, list<id> frontier` (frontier
/// ascending, so equal walk states give equal tokens).
fn encode_token(
    from: &ObjectId,
    path: &RepoPath,
    first_parent_only: bool,
    frontier: &[ObjectId],
) -> Vec<u8> {
    let mut sorted = frontier.to_vec();
    sorted.sort();
    let mut e = Encoder::new();
    e.uvarint(PAGE_TOKEN_VERSION);
    e.id(from);
    e.bytes(path.as_bytes());
    e.u8(u8::from(first_parent_only));
    e.uvarint(sorted.len() as u64);
    for id in &sorted {
        e.id(id);
    }
    e.finish()
}

/// Decode a token of [`encode_token`] and check that it belongs to this query.
fn decode_token(
    token: &[u8],
    from: &ObjectId,
    path: &RepoPath,
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    let parse = || -> Result<(ObjectId, Vec<u8>, u8, Vec<ObjectId>)> {
        let mut d = Decoder::new(token);
        if d.uvarint()? != PAGE_TOKEN_VERSION {
            return Err(bad_token("unknown page token version"));
        }
        let tf = d.id()?;
        let tp = d.bytes()?.to_vec();
        let flag = d.u8()?;
        let n = d.list_len()?;
        let mut frontier = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            frontier.push(d.id()?);
        }
        d.finish()?;
        Ok((tf, tp, flag, frontier))
    };
    let (tf, tp, flag, frontier) = parse().map_err(|e| {
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
    if tp != path.as_bytes() {
        return Err(bad_token("page token belongs to a different path"));
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
    if frontier.iter().any(|id| id.kind() != NODE_KIND) {
        return Err(bad_token("page token frontier holds a non-node id"));
    }
    Ok(frontier)
}

/// Decoded nodes and changeset generations of one walk.
struct NodeWalk<'a> {
    cx: &'a dyn DeriveCtx,
    nodes: HashMap<ObjectId, Arc<HistoryNode>>,
    generations: HashMap<ObjectId, u64>,
}

impl<'a> NodeWalk<'a> {
    fn new(cx: &'a dyn DeriveCtx) -> Self {
        NodeWalk {
            cx,
            nodes: HashMap::new(),
            generations: HashMap::new(),
        }
    }

    async fn node(&mut self, id: &ObjectId) -> Result<Arc<HistoryNode>> {
        if let Some(n) = self.nodes.get(id) {
            return Ok(n.clone());
        }
        let n = Arc::new(history::get_node(self.cx, id).await?);
        self.nodes.insert(*id, n.clone());
        Ok(n)
    }

    async fn generation(&mut self, cs: &ObjectId) -> Result<u64> {
        if let Some(g) = self.generations.get(cs) {
            return Ok(*g);
        }
        let g = self.cx.changeset(cs).await?.generation;
        self.generations.insert(*cs, g);
        Ok(g)
    }

    /// The heap entry `(generation, changeset, node)` of node `id`.
    async fn entry(&mut self, id: &ObjectId) -> Result<(u64, ObjectId, ObjectId)> {
        let cs = self.node(id).await?.changeset;
        let g = self.generation(&cs).await?;
        Ok((g, cs, *id))
    }
}

/// Changesets that changed `path`, walking history nodes from `from` (SPEC 7.4 Log with a
/// path, 9.2 TREESAME via node reuse), newest first: descending generation, ties by
/// descending changeset id. `first_parent_only` follows only each node's first parent node.
///
/// A path absent at `from` has an empty log. `page` is the token returned by the previous page
/// of the same query; the returned token is `None` when the walk is exhausted. A `limit` of 0
/// returns an empty page and a token for the unchanged position.
///
/// Errors: NOT_FOUND when `from` is missing; INVALID_ARGUMENT / `bad_page_token` for a
/// malformed token or one issued for a different `from`, path or `first_parent_only`.
pub async fn log_path(
    e: &DerivedEngine,
    from: &ObjectId,
    path: &RepoPath,
    first_parent_only: bool,
    limit: usize,
    page: Option<&[u8]>,
) -> Result<PathLogPage> {
    let cx = e.ctx();
    let mut w = NodeWalk::new(cx);
    let start: Vec<ObjectId> = match page {
        None => match history::node_for_path(cx, from, path).await? {
            Some((id, node)) => {
                w.nodes.insert(id, Arc::new(node));
                vec![id]
            }
            None => {
                return Ok(PathLogPage {
                    changesets: Vec::new(),
                    next_page: None,
                });
            }
        },
        Some(token) => {
            // `from` must exist even when continuing.
            cx.changeset(from).await?;
            let frontier = decode_token(token, from, path, first_parent_only)?;
            for id in &frontier {
                let node = match w.node(id).await {
                    Ok(n) => n,
                    Err(e) if e.code == ErrorCode::NotFound => {
                        return Err(bad_token("page token names an unknown node"));
                    }
                    Err(e) => return Err(e),
                };
                // Only nodes of this path may be continued from.
                let at = history::node_for_path(cx, &node.changeset, path).await?;
                if at.map(|(n, _)| n) != Some(*id) {
                    return Err(bad_token("page token names a node of another path"));
                }
            }
            frontier
        }
    };
    let mut heap: BinaryHeap<(u64, ObjectId, ObjectId)> = BinaryHeap::new();
    let mut queued: HashSet<ObjectId> = HashSet::new();
    for id in start {
        if queued.insert(id) {
            heap.push(w.entry(&id).await?);
        }
    }
    let mut out = Vec::with_capacity(limit.min(1024));
    while out.len() < limit {
        let Some((_, cs, id)) = heap.pop() else { break };
        queued.remove(&id);
        out.push(cs);
        let node = w.node(&id).await?;
        let parents: &[ObjectId] = if first_parent_only {
            node.parents.get(..1).unwrap_or(&[])
        } else {
            &node.parents
        };
        for p in parents {
            if queued.insert(*p) {
                heap.push(w.entry(p).await?);
            }
        }
    }
    let next_page = if heap.is_empty() {
        None
    } else {
        let frontier: Vec<ObjectId> = heap.iter().map(|(_, _, id)| *id).collect();
        Some(encode_token(from, path, first_parent_only, &frontier))
    };
    Ok(PathLogPage {
        changesets: out,
        next_page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    fn node(n: u8) -> ObjectId {
        ObjectId::new(NODE_KIND, [n; 32])
    }

    #[test]
    fn token_round_trip_and_validation() {
        let from = ObjectId::new(Kind::Changeset, [9; 32]);
        let path = RepoPath::parse(b"a/b").unwrap();
        let t = encode_token(&from, &path, true, &[node(3), node(1)]);
        assert_eq!(
            decode_token(&t, &from, &path, true).unwrap(),
            vec![node(1), node(3)]
        );
        let bad = |t: &[u8], f: &ObjectId, p: &RepoPath, fp: bool| {
            decode_token(t, f, p, fp).unwrap_err().subcode() == Some(BAD_PAGE_TOKEN)
        };
        assert!(bad(&t, &from, &path, false));
        assert!(bad(&t, &from, &RepoPath::parse(b"a").unwrap(), true));
        assert!(bad(
            &t,
            &ObjectId::new(Kind::Changeset, [8; 32]),
            &path,
            true
        ));
        assert!(bad(&t[..t.len() - 1], &from, &path, true));
        assert!(bad(b"", &from, &path, true));
        let empty = encode_token(&from, &path, true, &[]);
        assert!(bad(&empty, &from, &path, true));
        let wrong_kind = encode_token(&from, &path, true, &[from]);
        assert!(bad(&wrong_kind, &from, &path, true));
    }
}
