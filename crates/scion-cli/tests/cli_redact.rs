//! SPEC 19.13 through the CLI (P31): `redact --path --all-versions` finds every distinct blob
//! the path ever held (compared with the reference model's `all_versions` semantics on
//! generated histories), `log` and `show` render `<redacted>` after a Meta redaction with the
//! changeset ids unchanged, and every redact action produces audit entries with actor and
//! reason.
//!
//! The oracle mirrors `scion_refmodel::MRepo::all_versions` (BFS from the commit over all
//! parents, every distinct content the path holds as a file) on the generated structural
//! history; scion-cli has no direct dev-dependency on scion-refmodel (amendment requested).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod cli_support;

use cli_support::{Harness, scion};
use scion_conformance::{TestCluster, Via};
use scion_core::ObjectId;
use scion_testkit::{CorpusSpec, GenRepo, generate};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Reference-model `all_versions(c, p)`: distinct contents held at `p` by any commit reachable
/// from `c`, first-seen in BFS order (SPEC 13.2 "every distinct blob the path ever held").
fn model_all_versions(repo: &GenRepo, c: usize, p: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([c]);
    while let Some(x) = queue.pop_front() {
        if !seen.insert(x) {
            continue;
        }
        if let Some(f) = repo.commits[x].tree.get(p)
            && !out.contains(&f.content)
        {
            out.push(f.content.clone());
        }
        queue.extend(repo.commits[x].parents.iter().copied());
    }
    out
}

/// The path with the most distinct versions reachable from `c` (ties: smallest path).
fn busiest_path(repo: &GenRepo, c: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for gc in &repo.commits {
        paths.extend(gc.tree.keys().cloned());
    }
    let mut best: Option<(Vec<u8>, Vec<Vec<u8>>)> = None;
    for p in paths {
        let v = model_all_versions(repo, c, &p);
        if best.as_ref().is_none_or(|(_, b)| v.len() > b.len()) {
            best = Some((p, v));
        }
    }
    best.unwrap()
}

/// Audit rows `(action, target, actor, reason)` from the shared storage (PLAN 6.8).
async fn audit(h: &Harness) -> Vec<(String, String, String, String)> {
    h.cluster
        .storage
        .meta
        .audit_log(100_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.action, r.target, r.actor, r.reason))
        .collect()
}

async fn all_versions_case(h: &Harness, seed: u64, history_len: u32) {
    let repo = generate(&CorpusSpec {
        files: 10,
        depth: 2,
        fanout: 2,
        history_len,
        merge_ratio: 0.25,
        octopus_ratio: 0.05,
        binary_ratio: 0.1,
        symlink_ratio: 0.1,
        seed,
    });
    let ids = h.cluster.seed(&repo).await;
    let c = repo.commits.len() - 1;
    let (path, versions) = busiest_path(&repo, c);
    let path_s = String::from_utf8(path.clone()).unwrap();
    let expected: BTreeSet<String> = versions
        .iter()
        .map(|v| ObjectId::hash_blob(v).to_text())
        .collect();
    assert_eq!(expected.len(), versions.len());
    assert!(
        versions.len() >= 3,
        "seed {seed}: {path_s} has too few versions to be a test"
    );
    let reason = format!("all-versions seed {seed}");
    let admin = h.env_as("admin");
    let at = ids[c].to_text();
    let out = scion(
        &admin,
        &[
            "--json",
            "redact",
            "--path",
            &path_s,
            "--at",
            &at,
            "--all-versions",
            "--reason",
            &reason,
            "--purge-after",
            "30d",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "seed {seed}: {}", out.stderr);
    let v = out.json();
    cli_support::validate(&v);
    let targets: Vec<String> = v["data"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect();
    let got: BTreeSet<String> = targets.iter().cloned().collect();
    assert_eq!(got.len(), targets.len(), "seed {seed}: duplicate targets");
    assert_eq!(
        got, expected,
        "seed {seed}: --all-versions of {path_s} at commit {c} differs from the model"
    );
    // Every redacted id has an audit row with the caller and the reason.
    let rows = audit(h).await;
    for id in &got {
        assert!(
            rows.iter().any(|(a, t, who, r)| a == "redact"
                && t == id
                && who == "github:admin"
                && *r == reason),
            "seed {seed}: no audit row for {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn all_versions_matches_the_reference_model() {
    let h = Harness::start(false).await;
    let seeds = if scion_core::test_support::full_mode() {
        20
    } else {
        3
    };
    for seed in 0..seeds {
        all_versions_case(&h, 0x5C10 + seed, 40).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
async fn full_all_versions_matches_the_reference_model() {
    let h = Harness::start(false).await;
    for seed in 0..25 {
        all_versions_case(&h, 0xF011 + seed, 150).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn single_version_redaction_and_permissions() {
    let h = Harness::start(false).await;
    let admin_client = h.cluster.client(Via::Server, &TestCluster::seeder()).await;
    let head = h.main_head().await;
    let v1 = TestCluster::commit(
        &admin_client,
        &head,
        &[("s/key.txt", b"secret-1\n")],
        &[],
        1,
    )
    .await;
    let v2 = TestCluster::commit(&admin_client, &v1, &[("s/key.txt", b"secret-2\n")], &[], 2).await;
    // Not an admin: PERMISSION_DENIED.
    let alice = h.env_as("alice");
    let denied = scion(
        &alice,
        &["redact", "--path", "s/key.txt", "--at", &v2.to_text()],
    )
    .await;
    assert_eq!(denied.code, 1);
    assert!(
        denied.stderr.contains("PERMISSION_DENIED"),
        "{}",
        denied.stderr
    );
    // Without --all-versions only the blob at --at is redacted.
    let admin = h.env_as("admin");
    let out = scion(
        &admin,
        &[
            "--json",
            "redact",
            "--path",
            "s/key.txt",
            "--at",
            &v2.to_text(),
        ],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let v = out.json();
    assert_eq!(
        v["data"]["targets"],
        serde_json::json!([{"id": ObjectId::hash_blob(b"secret-2\n").to_text(), "kind": "blob"}])
    );
    assert_eq!(v["data"]["reason"], "redacted with scion redact");
    let old = scion(&alice, &["cat", &format!("{}:s/key.txt", v1.to_text())]).await;
    assert_eq!(
        (old.code, old.stdout.as_slice()),
        (0, b"secret-1\n".as_slice())
    );
    let new = scion(&alice, &["cat", &format!("{}:s/key.txt", v2.to_text())]).await;
    assert_eq!(new.code, 1);
    assert!(new.stderr.contains("REDACTED"), "{}", new.stderr);
    // Missing path: a local error before any RPC to the admin service.
    let missing = scion(
        &admin,
        &["redact", "--path", "nope.txt", "--at", &v2.to_text()],
    )
    .await;
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("does not exist"),
        "{}",
        missing.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn meta_redaction_renders_redacted_in_log_and_show() {
    let h = Harness::start(false).await;
    let client = h
        .cluster
        .client(Via::Server, &TestCluster::gh("alice"))
        .await;
    let head = h.main_head().await;
    let cs = TestCluster::commit(&client, &head, &[("m.txt", b"meta\n")], &[], 77).await;
    let alice = h.env_as("alice");
    let rev = cs.to_text();

    let before = scion(&alice, &["--json", "show", &rev]).await;
    assert_eq!(before.code, 0, "{}", before.stderr);
    let meta_id = before.json()["data"]["meta"]["meta_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let log = scion(&alice, &["log", "--rev", &rev, "--limit", "1"]).await;
    assert!(log.text().contains("    change 77\n"), "{}", log.text());
    assert!(!log.text().contains("<redacted>"));

    let admin = h.env_as("admin");
    let out = scion(
        &admin,
        &[
            "--json",
            "redact",
            "--meta",
            &rev,
            "--reason",
            "pii in message",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let v = out.json();
    cli_support::validate(&v);
    assert_eq!(
        v["data"]["targets"],
        serde_json::json!([{"id": meta_id, "kind": "meta"}])
    );

    let short: String = rev.chars().take(14).collect();
    let log = scion(&alice, &["log", "--rev", &rev, "--limit", "1"]).await;
    assert_eq!(log.code, 0, "{}", log.stderr);
    let text = log.text();
    assert!(text.contains(&format!("changeset {short}\n")), "{text}");
    assert!(text.contains("Author: <redacted>\n"), "{text}");
    assert!(text.contains("    <redacted>\n"), "{text}");
    assert!(!text.contains("change 77"), "{text}");
    let oneline = scion(&alice, &["log", "--rev", &rev, "--limit", "1", "--oneline"]).await;
    assert_eq!(oneline.text(), format!("{short} <redacted>\n"));
    let show = scion(&alice, &["show", &rev]).await;
    assert_eq!(show.code, 0, "{}", show.stderr);
    assert!(
        show.text().contains(&format!("changeset {short}\n")),
        "{}",
        show.text()
    );
    assert!(
        show.text().contains("Author: <redacted>"),
        "{}",
        show.text()
    );
    assert!(show.text().contains("A m.txt"), "{}", show.text());

    // JSON: redacted flag, only the meta id, same changeset id.
    let js = scion(&alice, &["--json", "show", &rev]).await;
    let v = js.json();
    cli_support::validate(&v);
    assert_eq!(v["data"]["id"], rev);
    assert_eq!(v["data"]["meta"]["redacted"], true);
    assert_eq!(v["data"]["meta"]["meta_id"], meta_id);
    assert!(v["data"]["meta"]["message"].is_null());
    let jl = scion(&alice, &["--json", "log", "--rev", &rev, "--limit", "1"]).await;
    let v = jl.json();
    assert_eq!(v["data"]["entries"][0]["id"], rev);
    assert_eq!(v["data"]["entries"][0]["meta"]["redacted"], true);

    let rows = audit(&h).await;
    assert!(rows.iter().any(|(a, t, who, r)| a == "redact"
        && *t == meta_id
        && who == "github:admin"
        && r == "pii in message"));
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_now_is_audited() {
    let h = Harness::start(false).await;
    let client = h
        .cluster
        .client(Via::Server, &TestCluster::gh("alice"))
        .await;
    let head = h.main_head().await;
    let cs = TestCluster::commit(&client, &head, &[("leak.txt", b"AKIA-leaked\n")], &[], 5).await;
    let blob = ObjectId::hash_blob(b"AKIA-leaked\n").to_text();
    let admin = h.env_as("admin");
    let out = scion(
        &admin,
        &[
            "redact",
            "--object",
            &blob,
            "--reason",
            "leaked key",
            "--purge-now",
        ],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.text().contains("1 object(s) purged"), "{}", out.text());
    let rows = audit(&h).await;
    let mine: BTreeMap<&str, usize> = rows
        .iter()
        .filter(|(_, t, who, r)| *t == blob && who == "github:admin" && r == "leaked key")
        .fold(BTreeMap::new(), |mut m, (a, ..)| {
            *m.entry(a.as_str()).or_default() += 1;
            m
        });
    assert_eq!(mine.get("redact"), Some(&1), "{rows:?}");
    assert_eq!(mine.get("purge"), Some(&1), "{rows:?}");
    let alice = h.env_as("alice");
    let cat = scion(&alice, &["cat", &format!("{}:leak.txt", cs.to_text())]).await;
    assert_eq!(cat.code, 1);
    assert!(cat.stderr.contains("REDACTED"), "{}", cat.stderr);
    // A tree id is not redactable (PLAN 6.7).
    let tree = scion(&alice, &["--json", "show", &cs.to_text()])
        .await
        .json()["data"]["root_tree"]
        .as_str()
        .unwrap()
        .to_owned();
    let bad = scion(&admin, &["redact", "--object", &tree]).await;
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("not_redactable"), "{}", bad.stderr);
}
