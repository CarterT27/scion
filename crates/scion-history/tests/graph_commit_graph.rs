//! SPEC 19.6 `commit_graph` (P13): generation and skip pointers, IsAncestor, first-parent
//! ancestry, MergeBase, `~n` / `^k` and paged Log, differentially against the reference model
//! (`scion_refmodel::MRepo`).
//!
//! DAGs are built by inserting `ChangesetRow`s directly into a `MemMetaStore`, with the
//! commit-graph columns computed by `compute_commit_graph`; no `create_changeset` is involved.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

use bytes::Bytes;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_core::test_support::{full_mode, proptest_cases};
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, SystemClock, empty_tree_id};
use scion_history::{History, HistoryConfig, NoEager, compute_commit_graph};
use scion_object::{Canonical, Changeset, Extensions};
use scion_refmodel::{MRepo, MTree};
use scion_store::{
    ChangesetBundle, ChangesetRow, MemHandles, MemOptions, MetaStore, ObjectRepo, ObjectRepoConfig,
    Storage,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

/// A DAG stored both in a `MemMetaStore` (through `History`) and in the reference model, with
/// identical node indices.
struct Dag {
    history: History,
    handles: MemHandles,
    ids: Vec<ObjectId>,
    index: HashMap<ObjectId, usize>,
    rows: HashMap<ObjectId, ChangesetRow>,
    model: MRepo,
    salt: u64,
}

impl Dag {
    fn new(salt: u64) -> Self {
        let (storage, handles) = Storage::in_memory(MemOptions::default());
        let objects = ObjectRepo::new(storage, ObjectRepoConfig::default());
        let history = History::new(
            objects,
            HistoryConfig::default(),
            Arc::new(NoEager),
            Arc::new(SystemClock),
        );
        Dag {
            history,
            handles,
            ids: Vec::new(),
            index: HashMap::new(),
            rows: HashMap::new(),
            model: MRepo::new(),
            salt,
        }
    }

    /// Add a changeset with the given parent indices (in order); returns its index.
    async fn add(&mut self, parents: &[usize]) -> usize {
        let n = self.ids.len();
        let parent_ids: Vec<ObjectId> = parents.iter().map(|&p| self.ids[p]).collect();
        let parent_rows: Vec<ChangesetRow> =
            parent_ids.iter().map(|p| self.rows[p].clone()).collect();
        let rows = &self.rows;
        let lookup = |c: &ObjectId, j: usize| rows.get(c).and_then(|r| r.skip.get(j).copied());
        let (generation, skip) = compute_commit_graph(parent_rows.first(), &parent_rows, lookup);
        let mut change = [0u8; 16];
        change[..8].copy_from_slice(&(n as u64).to_le_bytes());
        change[8..].copy_from_slice(&self.salt.to_le_bytes());
        let cs = Changeset {
            parents: parent_ids.clone(),
            root_tree: empty_tree_id(),
            meta: ObjectId::new(Kind::Meta, [7; 32]),
            change_id: ChangeId(change),
            delta: Vec::new(),
            extensions: Extensions::new(),
        };
        let encoded = cs.encode();
        let id = ObjectId::hash_encoded(Kind::Changeset, &encoded);
        let row = ChangesetRow {
            id,
            encoded: Bytes::from(encoded),
            generation,
            skip,
            change_id: cs.change_id,
            created_by: "service:test".into(),
            created_at: SystemTime::UNIX_EPOCH,
        };
        self.handles
            .meta
            .commit_changeset(ChangesetBundle {
                row: row.clone(),
                parents: parent_ids,
                object_paths: Vec::new(),
                derived: Vec::new(),
            })
            .await
            .unwrap();
        self.rows.insert(id, row);
        self.ids.push(id);
        self.index.insert(id, n);
        let m = self.model.commit(parents, MTree::new());
        assert_eq!(m, n);
        n
    }

    fn id(&self, i: usize) -> ObjectId {
        self.ids[i]
    }

    fn idx(&self, id: &ObjectId) -> usize {
        self.index[id]
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// The model's first-parent chain of `c`, starting with `c`.
    fn model_fp_chain(&self, c: usize) -> Vec<usize> {
        let mut out = vec![c];
        let mut cur = c;
        while let Some(&p) = self.model.parents(cur).first() {
            out.push(p);
            cur = p;
        }
        out
    }

    fn changesets_read(&self) -> u64 {
        self.history
            .objects()
            .storage()
            .counters
            .snapshot()
            .changesets_read
    }
}

/// Random DAG with multiple roots, merges, octopus merges and long first-parent runs.
async fn random_dag(rng: &mut ChaCha8Rng, n: usize, salt: u64) -> Dag {
    let mut dag = Dag::new(salt);
    for i in 0..n {
        if i == 0 || rng.random_bool(0.03) {
            dag.add(&[]).await;
            continue;
        }
        let roll = rng.random_range(0..100u32);
        let want = if roll < 68 {
            1
        } else if roll < 90 {
            2
        } else {
            rng.random_range(3..=6usize)
        }
        .min(i);
        let mut ps: Vec<usize> = Vec::new();
        // First parent: biased towards recent changesets (a "branch tip").
        let back = rng.random_range(0..i.min(8));
        ps.push(i - 1 - back);
        while ps.len() < want {
            let p = rng.random_range(0..i);
            if !ps.contains(&p) {
                ps.push(p);
            }
        }
        dag.add(&ps).await;
    }
    // A criss-cross on top of two independent tips so every DAG has an ambiguous pair.
    let x = dag.add(&[n - 1]).await;
    let y = dag.add(&[n / 2]).await;
    dag.add(&[x, y]).await;
    dag.add(&[y, x]).await;
    dag
}

/// Oracle data computed from the reference model once per DAG.
struct Oracle {
    ancestors: Vec<BTreeSet<usize>>,
    fp_chain: Vec<Vec<usize>>,
}

impl Oracle {
    fn new(dag: &Dag) -> Self {
        let n = dag.len();
        Oracle {
            ancestors: (0..n).map(|c| dag.model.ancestors(c)).collect(),
            fp_chain: (0..n).map(|c| dag.model_fp_chain(c)).collect(),
        }
    }

    /// Best common ancestors from the model's ancestor sets (same definition as
    /// `MRepo::merge_bases`).
    fn merge_bases(&self, a: usize, b: usize) -> Vec<usize> {
        let common: Vec<usize> = self.ancestors[a]
            .intersection(&self.ancestors[b])
            .copied()
            .collect();
        common
            .iter()
            .copied()
            .filter(|&c| {
                !common
                    .iter()
                    .any(|&d| d != c && self.ancestors[d].contains(&c))
            })
            .collect()
    }
}

fn pick_query(rng: &mut ChaCha8Rng, o: &Oracle, n: usize) -> (usize, usize) {
    let b = rng.random_range(0..n);
    let a = match rng.random_range(0..3u32) {
        0 => rng.random_range(0..n),
        1 => {
            let ch = &o.fp_chain[b];
            ch[rng.random_range(0..ch.len())]
        }
        _ => {
            let anc: Vec<usize> = o.ancestors[b].iter().copied().collect();
            anc[rng.random_range(0..anc.len())]
        }
    };
    if rng.random_bool(0.15) {
        (b, a)
    } else {
        (a, b)
    }
}

async fn check_graph_columns(dag: &Dag) {
    for i in 0..dag.len() {
        let (cs, row) = dag
            .history
            .objects()
            .get_changeset(&dag.id(i))
            .await
            .unwrap();
        assert_eq!(row.generation, dag.model.generation(i), "generation of {i}");
        let skip: Vec<usize> = row.skip.iter().map(|s| dag.idx(s)).collect();
        assert_eq!(skip, dag.model.skip(i), "skip of {i}");
        let parents: Vec<usize> = cs.parents.iter().map(|p| dag.idx(p)).collect();
        assert_eq!(parents, dag.model.parents(i));
    }
}

/// Run `total` random queries spread over several random DAGs.
async fn differential(total: usize, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let dags = 8usize;
    let per_dag = total.div_ceil(dags);
    // Coverage of answer classes: [anc true, anc false, fp true, fp false, 0 bases, 2+ bases].
    let mut seen = [0usize; 6];
    for d in 0..dags {
        let n = rng.random_range(40..220usize);
        let dag = random_dag(&mut rng, n, d as u64).await;
        check_graph_columns(&dag).await;
        let o = Oracle::new(&dag);
        let n = dag.len();
        for q in 0..per_dag {
            let (a, b) = pick_query(&mut rng, &o, n);
            let (ia, ib) = (dag.id(a), dag.id(b));
            let h = &dag.history;

            let want = o.ancestors[b].contains(&a);
            seen[usize::from(!want)] += 1;
            assert_eq!(
                h.is_ancestor(&ia, &ib).await.unwrap(),
                want,
                "is_ancestor({a},{b})"
            );
            if q % 64 == 0 {
                assert_eq!(dag.model.is_ancestor(a, b), want);
            }

            let want_fp = o.fp_chain[b].contains(&a);
            seen[2 + usize::from(!want_fp)] += 1;
            assert_eq!(
                h.is_first_parent_ancestor(&ia, &ib).await.unwrap(),
                want_fp,
                "is_first_parent_ancestor({a},{b})"
            );

            let mut want_mb: Vec<ObjectId> =
                o.merge_bases(a, b).into_iter().map(|c| dag.id(c)).collect();
            want_mb.sort();
            match want_mb.len() {
                0 => seen[4] += 1,
                1 => {}
                _ => seen[5] += 1,
            }
            if q % 16 == 0 {
                let mut direct: Vec<ObjectId> = dag
                    .model
                    .merge_bases(a, b)
                    .into_iter()
                    .map(|c| dag.id(c))
                    .collect();
                direct.sort();
                assert_eq!(direct, want_mb, "oracle self-check ({a},{b})");
            }
            assert_eq!(
                h.merge_bases(&ia, &ib).await.unwrap(),
                want_mb,
                "merge_bases({a},{b})"
            );
        }
    }
    if total >= 1000 {
        assert!(
            seen.iter().all(|&c| c > 0),
            "answer classes not covered: {seen:?}"
        );
    }
    eprintln!("answer classes: {seen:?}");
}

#[tokio::test]
async fn generation_and_skip_match_refmodel_for_random_dags() {
    let mut rng = ChaCha8Rng::seed_from_u64(0x9e37);
    for d in 0..6u64 {
        let n = rng.random_range(30..300usize);
        let dag = random_dag(&mut rng, n, d).await;
        check_graph_columns(&dag).await;
    }
}

#[tokio::test]
async fn queries_agree_with_refmodel() {
    differential(proptest_cases(10_000) as usize, 0x005c_1013).await;
}

#[tokio::test]
#[ignore]
async fn full_queries_agree_with_refmodel() {
    let total = if full_mode() {
        100_000
    } else {
        proptest_cases(10_000) as usize
    };
    differential(total, 0xf011).await;
}

#[tokio::test]
async fn criss_cross_has_two_merge_bases() {
    let mut dag = Dag::new(1);
    let r = dag.add(&[]).await;
    let x = dag.add(&[r]).await;
    let y = dag.add(&[r]).await;
    let m1 = dag.add(&[x, y]).await;
    let m2 = dag.add(&[y, x]).await;
    let c1 = dag.add(&[m1]).await;
    let c2 = dag.add(&[m2]).await;
    let h = &dag.history;
    let mut want = vec![dag.id(x), dag.id(y)];
    want.sort();
    assert_eq!(h.merge_bases(&dag.id(m1), &dag.id(m2)).await.unwrap(), want);
    assert_eq!(h.merge_bases(&dag.id(c1), &dag.id(c2)).await.unwrap(), want);
    assert_eq!(h.merge_bases(&dag.id(c2), &dag.id(c1)).await.unwrap(), want);
    // A single base, ancestor, self.
    assert_eq!(
        h.merge_bases(&dag.id(x), &dag.id(y)).await.unwrap(),
        vec![dag.id(r)]
    );
    assert_eq!(
        h.merge_bases(&dag.id(x), &dag.id(c1)).await.unwrap(),
        vec![dag.id(x)]
    );
    assert_eq!(
        h.merge_bases(&dag.id(c1), &dag.id(c1)).await.unwrap(),
        vec![dag.id(c1)]
    );
    assert_eq!(dag.model.merge_bases(m1, m2), vec![x, y]);
}

#[tokio::test]
async fn unrelated_roots_and_ancestry_basics() {
    let mut dag = Dag::new(2);
    let r1 = dag.add(&[]).await;
    let a1 = dag.add(&[r1]).await;
    let r2 = dag.add(&[]).await;
    let b1 = dag.add(&[r2]).await;
    // Octopus with both roots' lines.
    let m = dag.add(&[a1, b1]).await;
    let h = &dag.history;
    assert!(
        h.merge_bases(&dag.id(a1), &dag.id(b1))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.is_ancestor(&dag.id(r1), &dag.id(m)).await.unwrap());
    assert!(h.is_ancestor(&dag.id(r2), &dag.id(m)).await.unwrap());
    assert!(h.is_ancestor(&dag.id(m), &dag.id(m)).await.unwrap());
    assert!(!h.is_ancestor(&dag.id(m), &dag.id(r1)).await.unwrap());
    assert!(!h.is_ancestor(&dag.id(r1), &dag.id(b1)).await.unwrap());
    assert!(
        h.is_first_parent_ancestor(&dag.id(r1), &dag.id(m))
            .await
            .unwrap()
    );
    assert!(
        !h.is_first_parent_ancestor(&dag.id(r2), &dag.id(m))
            .await
            .unwrap()
    );
    assert!(
        h.is_first_parent_ancestor(&dag.id(m), &dag.id(m))
            .await
            .unwrap()
    );
    assert!(
        !h.is_first_parent_ancestor(&dag.id(m), &dag.id(a1))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn unknown_and_wrong_kind_ids_are_errors() {
    let mut dag = Dag::new(3);
    let r = dag.add(&[]).await;
    let h = &dag.history;
    let missing = ObjectId::new(Kind::Changeset, [0xab; 32]);
    let tree = empty_tree_id();
    let known = dag.id(r);
    assert_eq!(
        h.is_ancestor(&missing, &known).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.is_ancestor(&known, &missing).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.merge_bases(&known, &missing).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.nth_first_parent(&missing, 0).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.log_page(&missing, false, 10, None)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.is_first_parent_ancestor(&tree, &known)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        h.nth_parent(&tree, 1).await.unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[tokio::test]
async fn tilde_and_caret_edge_cases() {
    let mut dag = Dag::new(4);
    let mut chain = vec![dag.add(&[]).await];
    for _ in 0..40 {
        let p = *chain.last().unwrap();
        chain.push(dag.add(&[p]).await);
    }
    // A merge whose first parent is a short side branch off chain[3].
    let side = dag.add(&[chain[3]]).await;
    let tip = *chain.last().unwrap();
    let merge = dag.add(&[side, tip]).await;
    let octo_parents: Vec<usize> = vec![chain[10], chain[5], side, chain[0], chain[20]];
    let octo = dag.add(&octo_parents).await;
    let h = &dag.history;

    let tip_id = dag.id(tip);
    for n in 0..=(chain.len() as u32 + 3) {
        let want = chain
            .len()
            .checked_sub(1 + n as usize)
            .map(|i| dag.id(chain[i]));
        assert_eq!(h.nth_first_parent(&tip_id, n).await.unwrap(), want, "~{n}");
    }
    assert_eq!(h.nth_first_parent(&tip_id, u32::MAX).await.unwrap(), None);
    let root = dag.id(chain[0]);
    assert_eq!(h.nth_first_parent(&root, 0).await.unwrap(), Some(root));
    assert_eq!(h.nth_first_parent(&root, 1).await.unwrap(), None);
    assert_eq!(h.nth_parent(&root, 1).await.unwrap(), None);
    // `^0` is invalid (implementation-defined 6.6).
    let e = h.nth_parent(&root, 0).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some("invalid_revision"));

    // ~n follows first parents through merges: merge~1 = side, merge~2 = chain[3].
    let mid = dag.id(merge);
    assert_eq!(
        h.nth_first_parent(&mid, 1).await.unwrap(),
        Some(dag.id(side))
    );
    assert_eq!(
        h.nth_first_parent(&mid, 2).await.unwrap(),
        Some(dag.id(chain[3]))
    );
    assert_eq!(h.nth_first_parent(&mid, 6).await.unwrap(), None);
    assert_eq!(h.nth_parent(&mid, 2).await.unwrap(), Some(tip_id));
    assert_eq!(h.nth_parent(&mid, 3).await.unwrap(), None);

    let oid = dag.id(octo);
    for (k, p) in octo_parents.iter().enumerate() {
        assert_eq!(
            h.nth_parent(&oid, k as u32 + 1).await.unwrap(),
            Some(dag.id(*p))
        );
    }
    assert_eq!(h.nth_parent(&oid, 6).await.unwrap(), None);
    assert_eq!(h.nth_parent(&oid, u32::MAX).await.unwrap(), None);
    assert_eq!(
        h.nth_parent(&oid, 0).await.unwrap_err().code,
        ErrorCode::InvalidArgument
    );

    // Differential against the model chain for every node and several n.
    for c in 0..dag.len() {
        let ch = dag.model_fp_chain(c);
        for n in [0u32, 1, 2, 3, 5, 8, 13, 31, 32, 33, 64] {
            let want = ch.get(n as usize).map(|&i| dag.id(i));
            assert_eq!(h.nth_first_parent(&dag.id(c), n).await.unwrap(), want);
        }
    }
}

/// The full walk from the oracle: ancestors (or the first-parent chain) by descending
/// `(generation, id)`.
fn oracle_walk(dag: &Dag, o: &Oracle, from: usize, first_parent_only: bool) -> Vec<ObjectId> {
    let set: Vec<usize> = if first_parent_only {
        o.fp_chain[from].clone()
    } else {
        o.ancestors[from].iter().copied().collect()
    };
    let mut keyed: Vec<(u64, ObjectId)> = set
        .into_iter()
        .map(|c| (dag.model.generation(c), dag.id(c)))
        .collect();
    keyed.sort();
    keyed.reverse();
    keyed.into_iter().map(|(_, id)| id).collect()
}

async fn collect_pages(
    h: &History,
    from: &ObjectId,
    fp: bool,
    limit: usize,
) -> (Vec<ObjectId>, Vec<Vec<u8>>) {
    let mut out = Vec::new();
    let mut tokens = Vec::new();
    let mut token: Option<Vec<u8>> = None;
    loop {
        let (page, next) = h.log_page(from, fp, limit, token.as_deref()).await.unwrap();
        assert!(page.len() <= limit);
        out.extend(page.iter().copied());
        match next {
            Some(t) => {
                assert_eq!(page.len(), limit, "only the last page may be short");
                tokens.push(t.clone());
                token = Some(t);
            }
            None => break,
        }
    }
    (out, tokens)
}

#[tokio::test]
async fn log_paging_is_stable_and_complete() {
    let mut rng = ChaCha8Rng::seed_from_u64(0x106);
    for d in 0..4u64 {
        let n = rng.random_range(30..150usize);
        let mut dag = random_dag(&mut rng, n, 100 + d).await;
        let o = Oracle::new(&dag);
        let len = dag.len();
        for _ in 0..12 {
            let from = rng.random_range(0..len);
            let fid = dag.id(from);
            for fp in [false, true] {
                let want = oracle_walk(&dag, &o, from, fp);
                let (full, next) = dag
                    .history
                    .log_page(&fid, fp, usize::MAX, None)
                    .await
                    .unwrap();
                assert_eq!(full, want, "full walk from {from} fp={fp}");
                assert_eq!(next, None);
                let limit = rng.random_range(1..8usize);
                let (paged, tokens) = collect_pages(&dag.history, &fid, fp, limit).await;
                assert_eq!(paged, want, "paged walk from {from} fp={fp} limit={limit}");
                // Re-reading any page with the same token yields the same page.
                if let Some(t) = tokens.first() {
                    let p1 = dag
                        .history
                        .log_page(&fid, fp, limit, Some(t))
                        .await
                        .unwrap();
                    let p2 = dag
                        .history
                        .log_page(&fid, fp, limit, Some(t))
                        .await
                        .unwrap();
                    assert_eq!(p1, p2);
                    assert_eq!(&p1.0[..], &want[limit..(2 * limit).min(want.len())]);
                    // A token is bound to its query.
                    let e = dag
                        .history
                        .log_page(&fid, !fp, limit, Some(t))
                        .await
                        .unwrap_err();
                    assert_eq!(e.code, ErrorCode::InvalidArgument);
                    assert_eq!(e.subcode(), Some("bad_page_token"));
                }
            }
        }
        // Stability under concurrent writes: new descendants do not change pages from `from`.
        let from = rng.random_range(0..len);
        let fid = dag.id(from);
        let (first, token) = dag.history.log_page(&fid, false, 3, None).await.unwrap();
        for _ in 0..10 {
            let p = rng.random_range(0..dag.len());
            let q = rng.random_range(0..dag.len());
            if p == q {
                dag.add(&[p]).await;
            } else {
                dag.add(&[p, q]).await;
            }
        }
        let (again, token2) = dag.history.log_page(&fid, false, 3, None).await.unwrap();
        assert_eq!(first, again);
        assert_eq!(token, token2);
        let mut rest = first.clone();
        if let Some(t) = token {
            let (tail, _) = collect_pages_from(&dag.history, &fid, 5, t).await;
            rest.extend(tail);
        }
        assert_eq!(rest, oracle_walk(&dag, &Oracle::new(&dag), from, false));
    }
}

async fn collect_pages_from(
    h: &History,
    from: &ObjectId,
    limit: usize,
    token: Vec<u8>,
) -> (Vec<ObjectId>, usize) {
    let mut out = Vec::new();
    let mut token = Some(token);
    let mut pages = 0;
    while let Some(t) = token {
        let (page, next) = h.log_page(from, false, limit, Some(&t)).await.unwrap();
        out.extend(page);
        pages += 1;
        token = next;
    }
    (out, pages)
}

#[tokio::test]
async fn log_page_edge_cases() {
    let mut dag = Dag::new(5);
    let r = dag.add(&[]).await;
    let a = dag.add(&[r]).await;
    let b = dag.add(&[r]).await;
    let m = dag.add(&[a, b]).await;
    let h = &dag.history;
    let mid = dag.id(m);
    // Root: one element, no token.
    let (p, t) = h.log_page(&dag.id(r), false, 10, None).await.unwrap();
    assert_eq!(p, vec![dag.id(r)]);
    assert_eq!(t, None);
    // limit 0: empty page, a token for the unchanged position.
    let (p, t) = h.log_page(&mid, false, 0, None).await.unwrap();
    assert!(p.is_empty());
    let t = t.unwrap();
    let (p, _) = h.log_page(&mid, false, 1, Some(&t)).await.unwrap();
    assert_eq!(p, vec![mid]);
    // Exact fit: the last full page returns no token.
    let (p, t) = h.log_page(&mid, false, 4, None).await.unwrap();
    assert_eq!(p.len(), 4);
    assert_eq!(t, None);
    assert_eq!(p[0], mid);
    assert_eq!(*p.last().unwrap(), dag.id(r));
    // Equal-generation siblings are ordered by descending id.
    let (hi, lo) = if dag.id(a) > dag.id(b) {
        (dag.id(a), dag.id(b))
    } else {
        (dag.id(b), dag.id(a))
    };
    assert_eq!(p[1..3], [hi, lo]);
    // First-parent only.
    let (p, _) = h.log_page(&mid, true, 10, None).await.unwrap();
    assert_eq!(p, vec![mid, dag.id(a), dag.id(r)]);
    // Garbage and foreign tokens.
    for bad in [&b"nope"[..], &[][..]] {
        let e = h.log_page(&mid, false, 2, Some(bad)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }
    let (_, ta) = h.log_page(&dag.id(a), false, 1, None).await.unwrap();
    let e = h
        .log_page(&mid, false, 1, Some(&ta.unwrap()))
        .await
        .unwrap_err();
    assert_eq!(e.subcode(), Some("bad_page_token"));
}

#[tokio::test]
async fn skip_pointer_queries_are_logarithmic() {
    let mut dag = Dag::new(6);
    let mut prev = dag.add(&[]).await;
    let root = prev;
    for _ in 0..4095 {
        prev = dag.add(&[prev]).await;
    }
    let tip = dag.id(prev);
    let h = &dag.history;

    let before = dag.changesets_read();
    assert!(
        h.is_first_parent_ancestor(&dag.id(root), &tip)
            .await
            .unwrap()
    );
    let reads = dag.changesets_read() - before;
    // 13 skip levels scanned once in total plus one read per jump and the endpoints; a linear
    // walk reads 4096.
    assert!(
        reads <= 30,
        "is_first_parent_ancestor read {reads} changesets"
    );
    // Every distance (worst case: all bits set) stays logarithmic, for hits and misses.
    for target in [1usize, 2, 3, 1000, 2047, 2048, 3071, 4094] {
        let before = dag.changesets_read();
        assert!(
            h.is_first_parent_ancestor(&dag.id(target), &tip)
                .await
                .unwrap()
        );
        let reads = dag.changesets_read() - before;
        assert!(reads <= 30, "fp ancestor at {target}: {reads} reads");
    }

    let before = dag.changesets_read();
    assert!(
        !h.is_first_parent_ancestor(&tip, &dag.id(root))
            .await
            .unwrap()
    );
    assert!(dag.changesets_read() - before <= 4);

    let before = dag.changesets_read();
    assert_eq!(
        h.nth_first_parent(&tip, 4000).await.unwrap(),
        Some(dag.id(95))
    );
    let reads = dag.changesets_read() - before;
    assert!(reads <= 13, "nth_first_parent read {reads} changesets");

    // Generation pruning: a query whose ancestor candidate is newer than the descendant is
    // answered from the two endpoints alone.
    let other = dag.add(&[]).await;
    let h = &dag.history;
    assert!(!h.is_ancestor(&dag.id(other), &tip).await.unwrap());
    let before = dag.changesets_read();
    assert!(!h.is_ancestor(&tip, &dag.id(other)).await.unwrap());
    assert!(dag.changesets_read() - before <= 2);
    let before = dag.changesets_read();
    assert!(!h.is_ancestor(&tip, &dag.id(root)).await.unwrap());
    assert!(dag.changesets_read() - before <= 2);
}

/// First-parent ancestry stays logarithmic when merges make generations jump by more than one
/// per first-parent step, for both hits and misses (an off-chain `a` whose generation lies
/// inside the chain's range).
#[tokio::test]
async fn first_parent_ancestry_with_merges_is_logarithmic() {
    let mut dag = Dag::new(7);
    let root = dag.add(&[]).await;
    let mut main = vec![root];
    let mut side_tip = dag.add(&[root]).await;
    let mut sides = vec![side_tip];
    for i in 0..1024usize {
        // The side line grows twice as fast, so each merge raises the main generation by ~2.
        for _ in 0..2 {
            side_tip = dag.add(&[side_tip]).await;
            sides.push(side_tip);
        }
        let prev = *main.last().unwrap();
        let next = if i % 3 == 0 {
            dag.add(&[prev, side_tip]).await
        } else {
            dag.add(&[prev]).await
        };
        main.push(next);
    }
    let tip = *main.last().unwrap();
    let tip_id = dag.id(tip);
    let h = &dag.history;
    let chain = dag.model_fp_chain(tip);
    for &m in main.iter().step_by(37) {
        let before = dag.changesets_read();
        assert!(
            h.is_first_parent_ancestor(&dag.id(m), &tip_id)
                .await
                .unwrap()
        );
        let reads = dag.changesets_read() - before;
        assert!(reads <= 30, "fp hit: {reads} reads");
    }
    for &s in sides.iter().step_by(53) {
        assert!(!chain.contains(&s));
        let before = dag.changesets_read();
        assert!(
            !h.is_first_parent_ancestor(&dag.id(s), &tip_id)
                .await
                .unwrap()
        );
        let reads = dag.changesets_read() - before;
        assert!(reads <= 30, "fp miss: {reads} reads");
        // Full ancestry still sees merged side commits.
        assert_eq!(
            h.is_ancestor(&dag.id(s), &tip_id).await.unwrap(),
            dag.model.is_ancestor(s, tip)
        );
    }
}

/// A token whose frontier names an unknown changeset is rejected as `bad_page_token`.
#[tokio::test]
async fn log_page_token_with_unknown_frontier_is_rejected() {
    let mut dag = Dag::new(8);
    let r = dag.add(&[]).await;
    let a = dag.add(&[r]).await;
    let b = dag.add(&[a]).await;
    let h = &dag.history;
    let bid = dag.id(b);
    let (_, t) = h.log_page(&bid, false, 1, None).await.unwrap();
    let mut t = t.unwrap();
    // The token ends with the single frontier id, whose last bytes are its digest; corrupt the
    // digest so it names a changeset that does not exist.
    let n = t.len();
    t[n - 1] ^= 0xff;
    let e = h.log_page(&bid, false, 1, Some(&t)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some("bad_page_token"));
}
