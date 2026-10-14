//! SPEC 9.2 `blame` through a fake `DeriveCtx` (PLAN 2.7: blame is tested through a mock
//! context): `BlameType::deps` / `derive` driven by a small iterative driver over the history
//! nodes of generated corpora equal `MRepo::blame` (P24).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use bytes::Bytes;
use history_support::*;
use scion_core::ObjectId;
use scion_derived::LazyType;
use scion_derived::blame::{self, BlameRange, BlameType};
use scion_derived::history;
use scion_object::RepoPath;
use scion_testkit::{CorpusSpec, generate};
use std::collections::HashMap;

/// Derive the blame of `key` with an explicit work stack (deps first), memoizing values.
async fn derive(
    t: &BlameType,
    cx: &MockCtx,
    memo: &mut HashMap<Bytes, Bytes>,
    key: Bytes,
) -> Vec<BlameRange> {
    let mut stack: Vec<(Bytes, bool)> = vec![(key.clone(), false)];
    while let Some((k, expanded)) = stack.pop() {
        if memo.contains_key(&k) {
            continue;
        }
        let deps = t.deps(cx, &k).await.unwrap();
        if !expanded {
            stack.push((k.clone(), true));
            for d in deps.into_iter().rev() {
                if !memo.contains_key(&d) {
                    stack.push((d, false));
                }
            }
            continue;
        }
        let pairs: Vec<(Bytes, Bytes)> =
            deps.iter().map(|d| (d.clone(), memo[d].clone())).collect();
        let v = t.derive(cx, &k, &pairs).await.unwrap();
        // Deterministic: deriving again gives byte-identical values.
        assert_eq!(t.derive(cx, &k, &pairs).await.unwrap(), v);
        memo.insert(k, v);
    }
    blame::decode_ranges(&memo[&key]).unwrap()
}

#[tokio::test]
async fn blame_type_through_a_mock_ctx_matches_refmodel() {
    for seed in 0..3u64 {
        let repo = generate(&CorpusSpec {
            files: 25,
            depth: 3,
            fanout: 3,
            history_len: 30,
            merge_ratio: 0.3,
            octopus_ratio: 0.1,
            seed: 70 + seed,
            ..CorpusSpec::default()
        });
        let cx = MockCtx::new();
        let ids = cx.load(&repo).await;
        let model = model_of(&repo);
        let t = BlameType::new();
        let mut memo: HashMap<Bytes, Bytes> = HashMap::new();
        let n = repo.commits.len();
        for c in [n / 3, n / 2, n - 1] {
            for path in repo.commits[c].tree.keys() {
                let rp = RepoPath::parse(path).unwrap();
                let (node, _) = history::node_for_path(cx.as_ref(), &ids[c], &rp)
                    .await
                    .unwrap()
                    .unwrap();
                let got = derive(
                    &t,
                    cx.as_ref(),
                    &mut memo,
                    Bytes::copy_from_slice(node.as_bytes()),
                )
                .await;
                let want: Vec<BlameRange> = model
                    .blame(c, path)
                    .into_iter()
                    .map(|(s, l, o, os)| BlameRange {
                        start_line: s,
                        line_count: l,
                        origin_changeset: ids[o],
                        origin_start_line: os,
                    })
                    .collect();
                assert_eq!(got, want, "seed {seed} commit {c} {:?}", path);
            }
        }
    }
}

#[tokio::test]
async fn directory_nodes_and_bad_keys() {
    let repo = generate(&CorpusSpec {
        files: 10,
        depth: 2,
        fanout: 3,
        history_len: 5,
        seed: 1,
        ..CorpusSpec::default()
    });
    let cx = MockCtx::new();
    let ids = cx.load(&repo).await;
    let root = history::root_node(cx.as_ref(), ids.last().unwrap())
        .await
        .unwrap();
    let t = BlameType::new();
    // A directory node: no dependencies, empty blame.
    assert!(
        t.deps(cx.as_ref(), root.as_bytes())
            .await
            .unwrap()
            .is_empty()
    );
    let v = t.derive(cx.as_ref(), root.as_bytes(), &[]).await.unwrap();
    assert!(blame::decode_ranges(&v).unwrap().is_empty());
    // Keys that are not node ids are INVALID_ARGUMENT.
    let not_node = ids[0];
    assert!(t.deps(cx.as_ref(), not_node.as_bytes()).await.is_err());
    assert!(t.deps(cx.as_ref(), b"short").await.is_err());
    let blob = ObjectId::hash_blob(b"x");
    assert!(t.derive(cx.as_ref(), blob.as_bytes(), &[]).await.is_err());
}
