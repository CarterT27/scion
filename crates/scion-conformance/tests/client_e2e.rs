//! SPEC 19.10 client scenarios on the real stack (P33): every reusable scenario of
//! `scion_wc::testing` (P18: checkout, status, goto, sparse, crash recovery, sparse object set)
//! and `scion_wc::testing_write` (P28: commit, amend, conflict flow, land, write crash
//! recovery, redacted placeholders) runs against in-process servers, once directly
//! (`Via::Server`) and once through the daemon (`Via::Daemon`), on the Memory backend and, as
//! `external_*`, on CockroachDB + S3 when configured. Checkouts are compared with the
//! reference model's materialization. Every test ends with `invariants::check_16_2`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{Kind, ObjectId, Principal};
use scion_object::{RefName, RepoPath};
use scion_proto::v1;
use scion_refmodel::{MDelta, MTree, file_diff, materialize};
use scion_testkit::{CorpusSpec, generate};
use scion_wc::testing::{self as wct, Recovered, Scenario, Snapshot};
use std::collections::BTreeSet;
use std::sync::Arc;

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

/// A fast daemon redaction poll; the daemon's TTL ref cache is disabled because the write
/// scenarios resolve a ref right after landing on it, and a TTL cache may legitimately answer
/// with the previous target within its bound (SPEC 10.3).
fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend);
    o.daemon_tweak = Some(Arc::new(|d| {
        d.redaction_poll_interval_ms = 100;
        d.ref_cache_ttl_ms = 0;
    }));
    o
}

fn snapshot_tree(s: &Snapshot) -> MTree {
    s.iter()
        .map(|(p, (m, c))| {
            (
                p.as_bytes().to_vec(),
                scion_refmodel::MFile {
                    mode: *m as u8,
                    content: c.clone(),
                },
            )
        })
        .collect()
}

fn top_dirs(t: &MTree) -> Vec<Vec<u8>> {
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    for p in t.keys() {
        if let Some(i) = p.iter().position(|b| *b == b'/') {
            out.insert(p[..i].to_vec());
        }
    }
    out.into_iter().collect()
}

async fn create_ref(c: &TestCluster, name: &str, target: &ObjectId) {
    let admin = c.client(Via::Server, &gh("admin")).await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: target.as_bytes().to_vec(),
            reason: "client e2e".into(),
        })
        .await
        .unwrap();
}

/// `(tree id, [(entry name, entry id)])` of the directory `path` in changeset `cs`.
async fn listed_tree(
    client: &scion_proto::ScionClient,
    cs: &ObjectId,
    path: &[u8],
) -> (ObjectId, Vec<(Vec<u8>, ObjectId)>) {
    let r = client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: cs.as_bytes().to_vec(),
            path: path.to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let entries = r
        .entries
        .iter()
        .map(|e| {
            let info = e.entry.as_ref().unwrap();
            (e.name.clone(), ObjectId::from_bytes(&info.id).unwrap())
        })
        .collect();
    (ObjectId::from_bytes(&r.tree_id).unwrap(), entries)
}

async fn run_client_scenarios(backend: Backend, via: Via) {
    let c = TestCluster::start(opts(backend)).await;
    let v = via.as_str();
    let repo = generate(&CorpusSpec {
        files: 60,
        depth: 3,
        fanout: 3,
        history_len: 10,
        seed: 33,
        ..CorpusSpec::default()
    });
    let ids = c.seed(&repo).await;
    let (mid, last) = (repo.commits.len() / 2, repo.commits.len() - 1);
    create_ref(&c, "gen-a", &ids[mid]).await;
    create_ref(&c, "gen-b", &ids[last]).await;
    let direct = c.client(Via::Server, &gh("alice")).await;
    let v1c = TestCluster::commit(
        &direct,
        &ids[last],
        &[
            ("zz/touched.txt", b"one\n"),
            ("zz/unrelated.txt", b"same\n"),
        ],
        &[],
        900,
    )
    .await;
    let v2c = TestCluster::commit(&direct, &v1c, &[("zz/touched.txt", b"two\n")], &[], 901).await;
    create_ref(&c, "v1", &v1c).await;
    create_ref(&c, "v2", &v2c).await;
    // The write scenarios land on their own ref, starting from the generated history.
    create_ref(&c, "work", &ids[last]).await;

    let s = Scenario::new(c.client(via, &gh("alice")).await).unwrap();
    let tree_a = &repo.commits[mid].tree;
    let tree_b = &repo.commits[last].tree;

    // --- P18: checkout equals the model's materialization (full and sparse). ---
    for (rev, tree) in [("gen-a", tree_a), ("gen-b", tree_b)] {
        let snap = wct::checkout_snapshot(&s, rev, &[]).await.unwrap();
        assert_eq!(snapshot_tree(&snap), *tree, "{v} {rev}: full checkout");
        for dir in top_dirs(tree).into_iter().take(2) {
            let rp = RepoPath::parse(&dir).unwrap();
            let snap = wct::checkout_snapshot(&s, rev, &[rp]).await.unwrap();
            assert_eq!(
                snapshot_tree(&snap),
                materialize(tree, std::slice::from_ref(&dir)),
                "{v} {rev}: sparse checkout"
            );
        }
    }
    // Clean status with a warm cache: zero calls, nothing hashed.
    let (calls, hashed) = wct::clean_status_zero_calls(&s, "gen-b").await.unwrap();
    assert_eq!((calls, hashed), (0, 0), "{v}: clean status");
    // goto touches exactly the model's diff.
    let changed = wct::goto_touches_only_diff(&s, "gen-a", "gen-b")
        .await
        .unwrap();
    let mut want: Vec<RepoPath> = file_diff(tree_a, tree_b)
        .into_iter()
        .map(|d| match d {
            MDelta::Put(p, _) | MDelta::Delete(p) => RepoPath::parse(&p).unwrap(),
        })
        .collect();
    want.sort();
    assert_eq!(changed, want, "{v}: goto touched exactly the changed paths");
    wct::goto_dirty_protection(
        &s,
        "v1",
        "v2",
        &RepoPath::parse(b"zz/touched.txt").unwrap(),
        &RepoPath::parse(b"zz/unrelated.txt").unwrap(),
    )
    .await
    .unwrap();
    let dirs = top_dirs(tree_b);
    assert!(dirs.len() >= 2);
    wct::sparse_add_remove(
        &s,
        "gen-b",
        &RepoPath::parse(&dirs[0]).unwrap(),
        &RepoPath::parse(&dirs[1]).unwrap(),
    )
    .await
    .unwrap();
    for p in wct::materialization_points() {
        wct::goto_crash_recovery(&s, "gen-a", "gen-b", p)
            .await
            .unwrap_or_else(|e| panic!("{v}: goto crash recovery at {p}: {e}"));
    }
    // Sparse fetch: exactly the in-scope blobs.
    let scope = RepoPath::parse(&dirs[0]).unwrap();
    let fetched = wct::sparse_object_set(&s, "gen-b", &scope).await.unwrap();
    let inside: BTreeSet<ObjectId> = tree_b
        .iter()
        .filter(|(p, _)| p.starts_with(&dirs[0]) && p.get(dirs[0].len()) == Some(&b'/'))
        .map(|(_, f)| ObjectId::hash_blob(&f.content))
        .collect();
    let fetched_blobs: BTreeSet<ObjectId> = fetched
        .iter()
        .filter(|id| id.kind() == Kind::Blob)
        .copied()
        .collect();
    assert_eq!(fetched_blobs, inside, "{v}: sparse fetch object set");
    // ... and exactly the trees on the path to the scope (the root) plus every tree under it.
    let mut want_trees: BTreeSet<ObjectId> = BTreeSet::new();
    want_trees.insert(listed_tree(&direct, &ids[last], b"").await.0);
    let mut stack = vec![dirs[0].clone()];
    while let Some(path) = stack.pop() {
        let (tree, entries) = listed_tree(&direct, &ids[last], &path).await;
        want_trees.insert(tree);
        for (name, id) in entries {
            if id.kind() == Kind::Tree {
                let mut child = path.clone();
                child.push(b'/');
                child.extend_from_slice(&name);
                stack.push(child);
            }
        }
    }
    let fetched_trees: BTreeSet<ObjectId> = fetched
        .iter()
        .filter(|id| id.kind() == Kind::Tree)
        .copied()
        .collect();
    assert_eq!(fetched_trees, want_trees, "{v}: sparse fetch tree set");

    // --- P28: commit, amend, conflicts, land, crash recovery, placeholders. ---
    let work = RefName::parse("work").unwrap();
    let report = wct::commit_uploads_only_missing(&s, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: commit scenario: {e}"));
    assert!(report.uploaded > 0);
    wct::amend_preserves_change_id(&s, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: amend scenario: {e}"));
    wct::conflict_flow(&s, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: conflict flow: {e}"));
    wct::land_moves_base(&s, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: land scenario: {e}"));
    let outcomes = wct::crash_recovery_all(&s, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: write crash recovery: {e}"));
    assert_eq!(outcomes.len(), wct::write_points().len());
    for (p, o) in &outcomes {
        if p == scion_wc::points::REBASE_AFTER_CONFLICT_WRITE {
            assert_eq!(*o, Recovered::PausedOnConflicts, "{v}: {p}");
        }
    }
    let admin = c.client(Via::Server, &gh("admin")).await;
    wct::redacted_placeholder_commit(&s, &admin, &work)
        .await
        .unwrap_or_else(|e| panic!("{v}: placeholder scenario: {e}"));

    if via == Via::Daemon {
        let d = c.daemon.as_ref().unwrap();
        assert!(
            d.handle.stats().cache_hits > 0,
            "the daemon served from its cache"
        );
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn client_scenarios_directly_against_the_server() {
    run_client_scenarios(Backend::Memory, Via::Server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_scenarios_through_the_daemon() {
    run_client_scenarios(Backend::Memory, Via::Daemon).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_client_scenarios_directly_against_the_server() {
    if skip_external() {
        return;
    }
    run_client_scenarios(Backend::External, Via::Server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_client_scenarios_through_the_daemon() {
    if skip_external() {
        return;
    }
    run_client_scenarios(Backend::External, Via::Daemon).await;
}
