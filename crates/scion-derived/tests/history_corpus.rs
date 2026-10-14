//! SPEC 19.6 `history`: for every path of generated corpora (`scion_testkit::generate`),
//! walking history-node parents reproduces `scion_refmodel::MRepo::log_path`'s TREESAME
//! history (all parents and first-parent only). Run through the mock `DeriveCtx` over several
//! seeds and end to end through `History::create_changeset` with `DerivedEngine`
//! (`Registry::v1_default`) as the EagerHook. Spec-sized corpora run in `full_*` (SCION_FULL).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::test_support::proptest_cases;
use scion_testkit::{CorpusSpec, generate};

fn small_spec(seed: u64) -> CorpusSpec {
    CorpusSpec {
        files: 30,
        depth: 3,
        fanout: 3,
        history_len: 32,
        merge_ratio: 0.25,
        octopus_ratio: 0.08,
        seed,
        ..CorpusSpec::default()
    }
}

async fn mock_seed(spec: &CorpusSpec) -> usize {
    let repo = generate(spec);
    let cx = MockCtx::new();
    let ids = cx.load(&repo).await;
    check_against_model(cx.as_ref(), &repo, &ids).await
}

async fn stack_seed(spec: &CorpusSpec) -> usize {
    let repo = generate(spec);
    let s = stack();
    let ids = s.load(&repo).await;
    let checked = check_against_model(s.engine.ctx(), &repo, &ids).await;
    let v = s.fsck().await;
    assert!(v.is_empty(), "fsck violations: {v:?}");
    checked
}

#[tokio::test]
async fn mock_ctx_matches_refmodel_on_generated_corpora() {
    // SCION_PROPTEST_CASES scales the number of seeds (default 6).
    let seeds = u64::from(proptest_cases(6).max(1));
    let mut total = 0;
    for seed in 0..seeds {
        total += mock_seed(&small_spec(seed)).await;
    }
    assert!(total > 0);
}

#[tokio::test]
async fn end_to_end_matches_refmodel_on_a_generated_corpus() {
    let spec = CorpusSpec {
        files: 60,
        depth: 4,
        fanout: 3,
        history_len: 40,
        merge_ratio: 0.2,
        octopus_ratio: 0.07,
        seed: 0xC0FFEE,
        ..CorpusSpec::default()
    };
    assert!(stack_seed(&spec).await > 0);
}

#[tokio::test]
async fn end_to_end_and_mock_derive_identical_root_nodes() {
    let repo = generate(&small_spec(11));
    let s = stack();
    let ids = s.load(&repo).await;
    let cx = MockCtx::new();
    let mock_ids = cx.load(&repo).await;
    for (a, b) in ids.iter().zip(&mock_ids) {
        // Changeset ids differ (real Meta ids), so compare node shapes: the root node content
        // is the root tree, identical on both sides.
        let ra = scion_derived::history::root_node(s.engine.ctx(), a)
            .await
            .unwrap();
        let rb = scion_derived::history::root_node(cx.as_ref(), b)
            .await
            .unwrap();
        let na = scion_derived::history::get_node(s.engine.ctx(), &ra)
            .await
            .unwrap();
        let nb = scion_derived::history::get_node(cx.as_ref(), &rb)
            .await
            .unwrap();
        assert_eq!(na.content, nb.content);
        assert_eq!(na.kind, nb.kind);
        assert_eq!(na.children.len(), nb.children.len());
        assert_eq!(na.parents.len(), nb.parents.len());
    }
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): default corpora over many seeds"]
async fn full_matches_refmodel_on_default_corpora() {
    for seed in 0..20 {
        let spec = CorpusSpec {
            seed,
            ..CorpusSpec::default()
        };
        mock_seed(&spec).await;
    }
    for seed in 0..3 {
        let spec = CorpusSpec {
            seed: 100 + seed,
            ..CorpusSpec::default()
        };
        stack_seed(&spec).await;
    }
}
