//! SPEC 19.18 complexity gates end to end via RPC (P33).
//!
//! Every gate runs against a real server on `shared_subtree_repo` repositories of 10^3 and
//! 10^5 files (10^7 in the ignored `full_` test, run with `SCION_FULL=1`) with equal depth, and
//! asserts storage access counter deltas only (never wall-clock time):
//!
//! 1. CreateChangeset of a 1-file change reads at most `depth + 2` trees and writes at most
//!    `depth + 1` trees.
//! 2. Rebase over an unrelated tip reads zero blobs and O(|delta| x depth) trees.
//! 3. Land of a 1-file change performs a constant number of metadata transactions per attempt
//!    (fast-forward and rebasing lands).
//! 4. Sparse checkout of a fixed directory fetches the same object count at every size.
//! 5. Clean status reads zero objects from upstream (`ScionClient::calls`) and hashes only
//!    racily-clean or stat-changed files.
//! 6. Eager derivation of a 1-file change creates at most `depth + 1` history nodes.
//! 7. Object authorization reads `object_paths` rows only for the requested object.
//!
//! Every count is also required to be equal at every size. The repository is loaded as a
//! parentless fixture changeset written directly through the storage layer (its objects,
//! history nodes, `object_paths` rows and the changesets row), because 10^7-file deltas
//! exceed every RPC limit; all measured operations go through RPCs. The fixture's empty delta
//! is deliberately not a SPEC 4.3.4 delta, so these clusters are not checked with the SPEC
//! 16.2 oracle (fsck would flag the fixture itself).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via};
use scion_core::{ChangeId, Kind, Mode, ObjectId, Principal};
use scion_derived::history;
use scion_object::{
    Canonical, Changeset, Extensions, HistoryNode, Meta, Name, NodeKind, RepoPath, Timestamp,
};
use scion_proto::{ScionClient, v1};
use scion_store::{
    ChangesetBundle, ChangesetRow, CounterSnapshot, DerivedRow, ObjectPathRow, ObjectRepo,
};
use scion_testkit::{SharedSubtreeRepo, shared_subtree_repo};
use scion_wc::index::StatCache;
use scion_wc::{WcOptions, WorkingCopy};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

/// Depth (path components of every file) used at every size.
const DEPTH: u32 = 5;

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

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// No daemon (the counters are the server's storage counters), and the redaction refresher
/// slowed down so no background storage call interleaves with a measurement.
fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend).with_daemon(false);
    let base = o.server.tweak.clone();
    o.server.tweak = Some(Arc::new(move |cfg| {
        if let Some(b) = &base {
            b(cfg);
        }
        cfg.redaction.propagation_bound_ms = 3_600_000;
    }));
    o
}

// ---------------------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------------------

struct Fixture {
    c: TestCluster,
    repo: SharedSubtreeRepo,
    base: ObjectId,
    depth: u64,
}

async fn put_node(
    objects: &ObjectRepo,
    kind: NodeKind,
    content: ObjectId,
    cs: ObjectId,
    children: Vec<(Name, ObjectId)>,
) -> ObjectId {
    let node = HistoryNode {
        kind,
        content,
        changeset: cs,
        parents: Vec::new(),
        children,
        extensions: Extensions::new(),
    };
    objects
        .put_encoded(history::NODE_KIND, Bytes::from(node.encode()))
        .await
        .unwrap()
        .0
}

/// History nodes of the parentless changeset `cs` with tree `root` (identical subtrees give
/// identical nodes: O(distinct objects) work). Returns the root node.
async fn parentless_nodes(objects: &ObjectRepo, cs: ObjectId, root: ObjectId) -> ObjectId {
    let mut memo: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut stack: Vec<(ObjectId, bool)> = vec![(root, false)];
    while let Some((t, expanded)) = stack.pop() {
        if memo.contains_key(&t) {
            continue;
        }
        let tree = objects.get_tree(&t).await.unwrap();
        if !expanded {
            stack.push((t, true));
            for e in &tree.entries {
                if e.mode == Mode::Tree && !memo.contains_key(&e.id) {
                    stack.push((e.id, false));
                }
            }
            continue;
        }
        let mut children = Vec::with_capacity(tree.entries.len());
        for e in &tree.entries {
            let child = if e.mode == Mode::Tree {
                memo[&e.id]
            } else {
                match memo.get(&e.id) {
                    Some(n) => *n,
                    None => {
                        let n = put_node(objects, NodeKind::File, e.id, cs, Vec::new()).await;
                        memo.insert(e.id, n);
                        n
                    }
                }
            };
            children.push((e.name.clone(), child));
        }
        let n = put_node(objects, NodeKind::Dir, t, cs, children).await;
        memo.insert(t, n);
    }
    memo[&root]
}

/// One `object_paths` row per distinct object, at its first path in depth-first order
/// (the root tree at the empty path), so every fixture object is readable by `/` readers.
async fn fixture_paths(objects: &ObjectRepo, cs: ObjectId, root: ObjectId) -> Vec<ObjectPathRow> {
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut out = Vec::new();
    let mut stack: Vec<(ObjectId, RepoPath)> = vec![(root, RepoPath::root())];
    let row = |object: ObjectId, path: RepoPath| ObjectPathRow {
        object,
        path,
        first_changeset: cs,
        at: SystemTime::UNIX_EPOCH,
    };
    while let Some((t, path)) = stack.pop() {
        if !seen.insert(t) {
            continue;
        }
        out.push(row(t, path.clone()));
        let tree = objects.get_tree(&t).await.unwrap();
        for e in &tree.entries {
            let child = path.join(&e.name).unwrap();
            if e.mode == Mode::Tree {
                stack.push((e.id, child));
            } else if seen.insert(e.id) {
                out.push(row(e.id, child));
            }
        }
    }
    out
}

async fn load_fixture(backend: Backend, files: u64) -> Fixture {
    let c = TestCluster::start(opts(backend)).await;
    let repo = shared_subtree_repo(files, DEPTH);
    let objects = c.servers[0].ctx().objects.clone();
    for (id, kind, bytes) in repo.encoded_objects() {
        let stored = match kind {
            Kind::Blob => objects.put_blob(bytes).await.unwrap().0,
            k => objects.put_encoded(k, bytes).await.unwrap().0,
        };
        assert_eq!(stored, id);
    }
    let meta = Meta {
        author: b"Fixture <fixture@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"service:fixture".to_vec(),
        message: format!("shared-subtree fixture, {files} files").into_bytes(),
        extensions: Extensions::new(),
    };
    let (meta_id, _) = objects
        .put_encoded(Kind::Meta, Bytes::from(meta.encode()))
        .await
        .unwrap();
    let cs = Changeset {
        parents: Vec::new(),
        root_tree: repo.root_tree(),
        meta: meta_id,
        change_id: ChangeId([7; 16]),
        delta: Vec::new(),
        extensions: Extensions::new(),
    };
    let encoded = Bytes::from(cs.encode());
    let base = ObjectId::hash_encoded(Kind::Changeset, &encoded);
    let root_node = parentless_nodes(&objects, base, repo.root_tree()).await;
    let object_paths = fixture_paths(&objects, base, repo.root_tree()).await;
    c.storage
        .meta
        .commit_changeset(ChangesetBundle {
            row: ChangesetRow {
                id: base,
                encoded,
                generation: 1,
                skip: Vec::new(),
                change_id: cs.change_id,
                created_by: "service:fixture".to_owned(),
                created_at: SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths,
            derived: vec![DerivedRow {
                key: history::derived_key(&base),
                value: Some(Bytes::copy_from_slice(root_node.as_bytes())),
                storage_key: None,
                source_object: None,
                at: SystemTime::UNIX_EPOCH,
            }],
        })
        .await
        .unwrap();
    let admin = c.client(Via::Server, &gh("admin")).await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "gate".into(),
            target: idb(&base),
            reason: "complexity fixture".into(),
        })
        .await
        .unwrap();
    let depth = u64::from(repo.depth);
    Fixture {
        c,
        repo,
        base,
        depth,
    }
}

/// The last file path in path order (follows the last subdirectory at every level): a path
/// sharing only the root with [`SharedSubtreeRepo::perturb_path`] when the fanout exceeds 1.
fn last_path(repo: &SharedSubtreeRepo) -> RepoPath {
    let dirs = repo.structural_dirs();
    let mut node = repo.root_node();
    let mut path: Vec<u8> = Vec::new();
    loop {
        let (subdirs, files) = &dirs[node];
        if let Some((name, child)) = subdirs.last() {
            path.extend_from_slice(name);
            path.push(b'/');
            node = *child;
        } else {
            path.extend_from_slice(&files.last().unwrap().0);
            break;
        }
    }
    RepoPath::parse(&path).unwrap()
}

async fn measure<F, Fut, T>(c: &TestCluster, f: F) -> (T, CounterSnapshot)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let before = c.counters();
    let out = f().await;
    (out, c.counters() - before)
}

fn put_req(
    parent: &ObjectId,
    path: &RepoPath,
    content: &[u8],
    change: u32,
) -> v1::CreateChangesetRequest {
    TestCluster::create_request(
        parent,
        &[(std::str::from_utf8(path.as_bytes()).unwrap(), content)],
        &[],
        change,
    )
}

async fn create(client: &ScionClient, req: v1::CreateChangesetRequest) -> ObjectId {
    let r = client
        .write()
        .create_changeset(req)
        .await
        .unwrap()
        .into_inner();
    ObjectId::from_bytes(&r.changeset_id).unwrap()
}

async fn land(client: &ScionClient, head: &ObjectId) -> v1::LandResponse {
    client
        .write()
        .land(v1::LandRequest {
            r#ref: "gate".into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .unwrap()
        .into_inner()
}

fn land_attempts(metrics: &str) -> f64 {
    TestCluster::metric(metrics, "scion_land_attempts", &[]).unwrap_or(0.0)
}

/// The measured values of one repository size; every field must be equal at every size.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Gates {
    create_trees_read: u64,
    create_trees_written: u64,
    history_nodes: u64,
    rebase_trees_read: u64,
    land_ff_meta_txns: u64,
    land_rebase_meta_txns: u64,
    sparse_objects: usize,
    sparse_object_reads: u64,
    auth_rows_read: u64,
}

async fn run_gates(backend: Backend, files: u64) -> Gates {
    let fx = load_fixture(backend, files).await;
    let (c, depth, base) = (&fx.c, fx.depth, fx.base);
    let alice = c.client(Via::Server, &gh("alice")).await;
    let perturb = fx.repo.perturb_path();
    let other = last_path(&fx.repo);
    assert_ne!(perturb, other);
    let shared_root = perturb
        .components()
        .next()
        .zip(other.components().next())
        .is_some_and(|(a, b)| a == b);
    assert!(
        !shared_root || fx.repo.fanout == 1,
        "the paths diverge at the root"
    );
    let msg = |g: &str| format!("{files} files, depth {depth}: {g}");

    // --- Gates 1 and 6: CreateChangeset of a 1-file change; eager history nodes. ---
    let content_a = b"gate: perturbed content\n".to_vec();
    TestCluster::upload(&alice, &[&content_a]).await;
    let (a, d) = measure(c, || {
        create(&alice, put_req(&base, &perturb, &content_a, 1))
    })
    .await;
    eprintln!("19.18 [{}] create: {d:?}", msg(""));
    assert!(
        d.trees_read <= depth + 2,
        "{}",
        msg(&format!("create read {} trees", d.trees_read))
    );
    assert!(
        d.trees_written <= depth + 1,
        "{}",
        msg(&format!("create wrote {} trees", d.trees_written))
    );
    assert_eq!(d.blobs_read, 0, "{}", msg("create read blob content"));
    // Objects written: the new trees, one Meta and the history nodes (SPEC 9.2).
    let history_nodes = d.objects_written - d.trees_written - 1;
    assert!(
        history_nodes >= 1 && history_nodes <= depth + 1,
        "{}",
        msg(&format!("{history_nodes} history nodes"))
    );
    let (create_trees_read, create_trees_written) = (d.trees_read, d.trees_written);

    // --- Gate 2: Rebase over an unrelated tip. ---
    let content_b = b"gate: unrelated tip\n".to_vec();
    TestCluster::upload(&alice, &[&content_b]).await;
    let b = create(&alice, put_req(&base, &other, &content_b, 2)).await;
    let (rebased, d) = measure(c, || async {
        alice
            .write()
            .rebase(v1::RebaseRequest {
                changeset: idb(&a),
                onto: idb(&b),
            })
            .await
            .unwrap()
            .into_inner()
    })
    .await;
    eprintln!("19.18 [{}] rebase: {d:?}", msg(""));
    assert_ne!(rebased.changeset_id, idb(&a));
    assert_eq!(d.blobs_read, 0, "{}", msg("rebase read blob content"));
    // |delta| = 1: base, ours and theirs along the changed path, plus the merged path.
    assert!(
        d.trees_read <= 4 * (depth + 1),
        "{}",
        msg(&format!("rebase read {} trees", d.trees_read))
    );
    let rebase_trees_read = d.trees_read;

    // --- Gate 3: Land of a 1-file change; constant metadata transactions per attempt. ---
    let mut ff = Vec::new();
    let mut rebasing = Vec::new();
    for i in 0..3u32 {
        let (tip, _) = c.ref_head("gate").await;
        let content = format!("gate: land ff {i}\n").into_bytes();
        TestCluster::upload(&alice, &[&content]).await;
        let path = RepoPath::parse(format!("land/ff{i}.txt").as_bytes()).unwrap();
        let head = create(&alice, put_req(&tip, &path, &content, 10 + i)).await;
        let m0 = c.server_metrics(0).await;
        let (r, d) = measure(c, || land(&alice, &head)).await;
        let attempts = land_attempts(&c.server_metrics(0).await) - land_attempts(&m0);
        assert_eq!(attempts, 1.0, "{}", msg("one land attempt"));
        assert_eq!(r.target, idb(&head), "fast-forward");
        ff.push(d.meta_txns);

        // A land that must rebase: the head is based on a tip that moved meanwhile.
        let (tip, _) = c.ref_head("gate").await;
        let mine_c = format!("gate: land mine {i}\n").into_bytes();
        let theirs_c = format!("gate: land theirs {i}\n").into_bytes();
        TestCluster::upload(&alice, &[&mine_c, &theirs_c]).await;
        let mine_p = RepoPath::parse(format!("land/mine{i}.txt").as_bytes()).unwrap();
        let theirs_p = RepoPath::parse(format!("land/theirs{i}.txt").as_bytes()).unwrap();
        let mine = create(&alice, put_req(&tip, &mine_p, &mine_c, 20 + i)).await;
        let theirs = create(&alice, put_req(&tip, &theirs_p, &theirs_c, 30 + i)).await;
        land(&alice, &theirs).await;
        let m0 = c.server_metrics(0).await;
        let (r, d) = measure(c, || land(&alice, &mine)).await;
        let attempts = land_attempts(&c.server_metrics(0).await) - land_attempts(&m0);
        assert_eq!(attempts, 1.0, "{}", msg("one land attempt"));
        assert_ne!(r.target, idb(&mine), "rebased");
        rebasing.push(d.meta_txns);
    }
    eprintln!(
        "19.18 [{}] land meta_txns: ff {ff:?}, rebasing {rebasing:?}",
        msg("")
    );
    // Background work can only add transactions; the minimum is the operation's own cost.
    let land_ff_meta_txns = *ff.iter().min().unwrap();
    let land_rebase_meta_txns = *rebasing.iter().min().unwrap();

    // --- Gates 4 and 5: sparse checkout of a fixed directory; clean status. ---
    let (tip, _) = c.ref_head("gate").await;
    let fixed: [(&str, &[u8]); 3] = [
        ("fixed/a.txt", b"fixed a\n"),
        ("fixed/sub/b.txt", b"fixed b\n"),
        ("fixed/sub/c.txt", b"fixed c\n"),
    ];
    let fixed_cs = TestCluster::commit(&alice, &tip, &fixed, &[], 40).await;
    let dir = tempfile::tempdir().unwrap();
    let wc_root = dir.path().join("wc");
    let client = c.client(Via::Server, &gh("alice")).await;
    let rev = fixed_cs.to_text();
    let (wc, d) = measure(c, || {
        WorkingCopy::checkout(
            client.clone(),
            &wc_root,
            &rev,
            vec![RepoPath::parse(b"fixed").unwrap()],
            WcOptions::default(),
        )
    })
    .await;
    let mut wc = wc.unwrap();
    let fetched = wc.fetched_objects();
    eprintln!(
        "19.18 [{}] sparse checkout: {} objects, {d:?}",
        msg(""),
        fetched.len()
    );
    // The root tree, fixed/, fixed/sub/ and three blobs.
    assert_eq!(fetched.len(), 6, "{}", msg("sparse checkout object set"));
    assert_eq!(fetched.iter().filter(|i| i.kind() == Kind::Blob).count(), 3);
    let sparse_object_reads = d.trees_read + d.blobs_read;

    let first = wc.status().await.unwrap();
    assert!(first.is_clean());
    let scion_dir = wc_root.join(".scion");
    let racy = |idx: &StatCache| idx.entries.values().filter(|e| idx.is_racy(e)).count();
    let idx = StatCache::load(&scion_dir).unwrap();
    let racy_now = racy(&idx);
    let calls = client.calls();
    let (st, d) = measure(c, || wc.status()).await;
    let st = st.unwrap();
    assert!(st.is_clean());
    assert_eq!(
        client.calls() - calls,
        0,
        "{}",
        msg("clean status made upstream calls")
    );
    assert_eq!(
        d.trees_read + d.blobs_read + d.meta_txns,
        0,
        "{}",
        msg("clean status read storage")
    );
    assert!(
        st.hashed <= racy_now,
        "{}",
        msg(&format!(
            "clean status hashed {} (racy {racy_now})",
            st.hashed
        ))
    );
    // Stat changes: new content in a.txt, the same content rewritten in b.txt.
    std::fs::write(wc_root.join("fixed/a.txt"), b"fixed a, edited locally\n").unwrap();
    std::fs::remove_file(wc_root.join("fixed/sub/b.txt")).unwrap();
    std::fs::write(wc_root.join("fixed/sub/b.txt"), b"fixed b\n").unwrap();
    let idx = StatCache::load(&scion_dir).unwrap();
    let racy_others = idx
        .entries
        .iter()
        .filter(|(p, e)| {
            idx.is_racy(e) && !p.as_bytes().ends_with(b"a.txt") && !p.as_bytes().ends_with(b"b.txt")
        })
        .count();
    let calls = client.calls();
    let st = wc.status().await.unwrap();
    assert_eq!(
        client.calls() - calls,
        0,
        "{}",
        msg("status made upstream calls")
    );
    assert_eq!(st.modified, vec![RepoPath::parse(b"fixed/a.txt").unwrap()]);
    assert!(
        st.hashed >= 2 && st.hashed <= 2 + racy_others,
        "{}",
        msg(&format!(
            "status hashed {} (stat-changed 2, racy {racy_others})",
            st.hashed
        ))
    );

    // --- Gate 7: object authorization reads object_paths rows only for the requested
    // object. bob has read nothing yet, so no decision is cached. ---
    let bob = c.client(Via::Server, &gh("bob")).await;
    let blob_a = ObjectId::hash_blob(&content_a);
    let mut auth_rows_read = 0;
    for id in [blob_a, fx.repo.root_tree()] {
        let rows = c
            .storage
            .meta
            .object_paths(&id, 1_000_000)
            .await
            .unwrap()
            .len() as u64;
        assert!(rows >= 1);
        let ids = [id];
        let (f, d) = measure(c, || TestCluster::fetch(&bob, &ids, None, None)).await;
        assert!(f.unwrap().items[0].verified);
        assert!(
            d.object_paths_rows_read >= 1 && d.object_paths_rows_read <= rows,
            "{}",
            msg(&format!(
                "authorizing {id} read {} object_paths rows ({rows} exist)",
                d.object_paths_rows_read
            ))
        );
        auth_rows_read += d.object_paths_rows_read;
    }

    Gates {
        create_trees_read,
        create_trees_written,
        history_nodes,
        rebase_trees_read,
        land_ff_meta_txns,
        land_rebase_meta_txns,
        sparse_objects: fetched.len(),
        sparse_object_reads,
        auth_rows_read,
    }
}

async fn gates_are_size_independent(backend: Backend, sizes: &[u64]) {
    let mut first: Option<(u64, Gates)> = None;
    for &files in sizes {
        let g = run_gates(backend.clone(), files).await;
        eprintln!("19.18 gates at {files} files: {g:?}");
        match &first {
            None => first = Some((files, g)),
            Some((f0, g0)) => assert_eq!(&g, g0, "gates at {files} files differ from {f0} files"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gates_at_1e3_and_1e5_files() {
    gates_are_size_independent(Backend::Memory, &[1_000, 100_000]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_gates_at_1e3_and_1e5_files() {
    if skip_external() {
        return;
    }
    gates_are_size_independent(Backend::External, &[1_000, 100_000]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spec-sized (SCION_FULL=1): 10^7 files"]
async fn full_gates_at_1e3_1e5_and_1e7_files() {
    if !scion_core::test_support::full_mode() {
        println!("SKIP: SCION_FULL unset");
        return;
    }
    gates_are_size_independent(Backend::Memory, &[1_000, 100_000, 10_000_000]).await;
}
