//! SPEC 19.13 redaction end to end (P33): the full runbook through the real stack (two servers
//! over one storage and a daemon in front of the first), on the Memory backend and, as
//! `external_*`, on CockroachDB + S3 when configured.
//!
//! - Redacting a blob changes no tree, changeset or ref id.
//! - Every server returns `redacted` by `effective_by` (the redacting server and the other
//!   one, which learns through its refresher); a daemon returns `redacted` within
//!   `redaction_poll_interval_ms` plus one request and deletes its cached copy.
//! - Re-introduction by upload or `Put` fails REDACTED; carrying the entry forward succeeds;
//!   a rebase needing the redacted content reports a `redacted` conflict.
//! - Meta redaction: `Log` renders the Meta as redacted; changeset ids unchanged.
//! - Unredact from quarantine restores reads byte-identically; purge deletes the live and
//!   quarantine bytes; unredact after purge fails; the audit log records every action.
//! - Daemon fail-closed: with polling blocked beyond `redaction_max_staleness_ms` the daemon
//!   stops serving from cache and forwards reads; it recovers when polling resumes.
//! - Working copies: placeholders on checkout (clean in status, editable); newly redacted clean
//!   files are replaced on the next mutating command; dirty files are untouched and reported.
//!
//! Every test ends with `invariants::check_16_2`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ConflictKind, ErrorCode, ErrorPayload, ObjectId, Principal};
use scion_object::{RefName, RepoPath};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_store::{Area, Consistency, ObjectState};
use scion_wc::testing::{self as wct, Scenario};
use scion_wc::{WcOptions, WorkingCopy, placeholder_bytes};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Daemon poll interval used by this suite (SPEC 6.3 `redaction_poll_interval_ms`).
const POLL_MS: u64 = 400;
/// Daemon staleness bound used by this suite (SPEC 6.3 `redaction_max_staleness_ms`).
const STALENESS_MS: u64 = 1500;
/// Scheduling slack added to every bound.
const SLACK_MS: u64 = 150;

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

fn tag() -> String {
    scion_core::ChangeId::random().to_hex()
}

fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend).with_servers(2);
    o.daemon_tweak = Some(Arc::new(|d| {
        d.redaction_poll_interval_ms = POLL_MS;
        d.redaction_max_staleness_ms = STALENESS_MS;
        // Ref reads through the daemon must see lands immediately in the working-copy part.
        d.ref_cache_ttl_ms = 0;
    }));
    o
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn sleep_until_ms(unix_ms: i64) {
    let d = unix_ms - now_ms();
    if d > 0 {
        tokio::time::sleep(Duration::from_millis(d as u64)).await;
    }
}

/// Poll `f` until it holds (at most 20 s).
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn land(client: &ScionClient, r: &str, head: &ObjectId) -> v1::LandResponse {
    client
        .write()
        .land(v1::LandRequest {
            r#ref: r.into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .unwrap()
        .into_inner()
}

async fn redact(admin: &ScionClient, ids: &[ObjectId], reason: &str) -> v1::RedactResponse {
    admin
        .admin()
        .redact(v1::RedactRequest {
            object_ids: ids.iter().map(idb).collect(),
            reason: reason.into(),
            purge_after_seconds: None,
        })
        .await
        .unwrap()
        .into_inner()
}

/// The status of one object read through `client`; `(status, content)`.
async fn read_status(client: &ScionClient, id: &ObjectId) -> (i32, Vec<u8>) {
    let f = TestCluster::fetch(client, &[*id], None, None)
        .await
        .unwrap();
    (f.items[0].status, f.items[0].content.clone())
}

const OK: i32 = v1::ItemStatus::Ok as i32;
const REDACTED: i32 = v1::ItemStatus::Redacted as i32;

async fn list_tree(client: &ScionClient, cs: &ObjectId, path: &str) -> v1::ListTreeResponse {
    client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(cs),
            path: path.as_bytes().to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
}

async fn changesets(client: &ScionClient, ids: &[ObjectId]) -> Vec<v1::ChangesetItem> {
    client
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: ids.iter().map(idb).collect(),
        })
        .await
        .unwrap()
        .into_inner()
        .items
}

async fn ref_log(client: &ScionClient, r: &str) -> Vec<v1::RefLogEntryMsg> {
    client
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: r.into(),
            from_version: 0,
            limit: 1000,
        })
        .await
        .unwrap()
        .into_inner()
        .entries
}

/// Every id visible from the outside that redaction must not change.
async fn identity_snapshot(client: &ScionClient, cs: &[ObjectId]) -> String {
    let mut out = String::new();
    for c in cs {
        out.push_str(&format!("{:?}\n", list_tree(client, c, "").await));
        out.push_str(&format!("{:?}\n", list_tree(client, c, "app").await));
    }
    out.push_str(&format!("{:?}\n", changesets(client, cs).await));
    out.push_str(&format!("{:?}\n", ref_log(client, "main").await));
    out
}

// ---------------------------------------------------------------------------------------
// The runbook.
// ---------------------------------------------------------------------------------------

async fn run_runbook(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let admin = c.client(Via::Server, &gh("admin")).await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let alice_1 = ScionClient::connect(
        &c.servers[1].endpoint,
        c.servers[1].tls.clone(),
        Some(c.token_for(&gh("alice"))),
    )
    .await
    .unwrap();
    let (root, _) = c.ref_head("main").await;
    let secret = format!("AKIA leaked credential {t}\n").into_bytes();
    let keep = format!("keep me {t}\n").into_bytes();
    let cfg1 = format!("config v1 {t}\n").into_bytes();
    let cfg2 = format!("config v2 {t}\n").into_bytes();
    let c1 = TestCluster::commit(
        &alice,
        &root,
        &[
            ("app/secret.txt", &secret),
            ("app/keep.txt", &keep),
            ("app/config.txt", &cfg1),
        ],
        &[],
        1,
    )
    .await;
    land(&alice, "main", &c1).await;
    let c2 = TestCluster::commit(&alice, &c1, &[("app/config.txt", &cfg2)], &[], 2).await;
    land(&alice, "main", &c2).await;
    let sid = ObjectId::hash_blob(&secret);
    let kid = ObjectId::hash_blob(&keep);

    // The daemon caches the secret.
    assert_eq!(read_status(&alice_d, &sid).await, (OK, secret.clone()));
    let d = c.daemon.as_ref().unwrap();
    assert!(d.handle.cache().contains(&sid));
    let before = identity_snapshot(&alice, &[c1, c2]).await;

    // --- Redact. ---
    let redacted_at = now_ms();
    let r = redact(&admin, &[sid], "leaked credential").await;
    assert!(r.epoch > 0);
    let bound = c.servers[0].config().redaction.propagation_bound_ms as i64;
    assert!(
        r.effective_by_unix_ms >= redacted_at && r.effective_by_unix_ms <= now_ms() + bound,
        "effective_by = commit time + propagation bound"
    );

    // Every server returns `redacted` by effective_by.
    sleep_until_ms(r.effective_by_unix_ms).await;
    for (name, client) in [("server 0", &alice), ("server 1", &alice_1)] {
        assert_eq!(
            read_status(client, &sid).await.0,
            REDACTED,
            "{name} after effective_by"
        );
    }
    // Tree entries still carry the id and size (SPEC 13.4).
    let app = list_tree(&alice, &c2, "app").await;
    let entry = app
        .entries
        .iter()
        .find(|e| e.name == b"secret.txt")
        .unwrap();
    assert_eq!(
        {
            let e = entry.entry.as_ref().unwrap();
            (e.id.clone(), e.size)
        },
        (idb(&sid), secret.len() as u64)
    );

    // The daemon: within one poll interval plus one request, with its cached copy deleted.
    sleep_until_ms(redacted_at + (POLL_MS + SLACK_MS) as i64).await;
    let mut stale = 0;
    for _ in 0..2 {
        match read_status(&alice_d, &sid).await {
            (s, _) if s == REDACTED => {}
            (s, content) => {
                assert_eq!((s, content), (OK, secret.clone()));
                stale += 1;
            }
        }
    }
    assert!(
        stale <= 1,
        "the daemon served a redacted object for {stale} requests"
    );
    assert_eq!(read_status(&alice_d, &sid).await.0, REDACTED);
    eventually("the daemon deletes its cached copy", || {
        !d.handle.cache().contains(&sid)
    })
    .await;

    // No identifier changed.
    assert_eq!(
        identity_snapshot(&alice, &[c1, c2]).await,
        before,
        "ids unchanged"
    );

    // --- Re-introduction is blocked; carry-forward is allowed. ---
    for (via, client) in [("server", &alice), ("daemon", &alice_d)] {
        let resp = client
            .write()
            .upload_objects(futures::stream::iter(vec![v1::UploadItem {
                id: idb(&sid),
                kind: u32::from(scion_core::Kind::Blob.byte()),
                data: secret.clone(),
            }]))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            resp.results[0].status, REDACTED,
            "{via}: upload of redacted content"
        );
        let e = client
            .write()
            .create_changeset(TestCluster::create_request(
                &c2,
                &[("app/copy-of-secret.txt", &secret)],
                &[],
                3,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            error_from_status(&e).code,
            ErrorCode::Redacted,
            "{via}: Put of a redacted id"
        );
    }
    let keep2 = format!("keep v2 {t}\n").into_bytes();
    let c3 = TestCluster::commit(&alice, &c2, &[("app/keep.txt", &keep2)], &[], 4).await;
    let carried = list_tree(&alice, &c3, "app").await;
    let entry = carried
        .entries
        .iter()
        .find(|e| e.name == b"secret.txt")
        .unwrap();
    assert_eq!(
        entry.entry.as_ref().unwrap().id,
        idb(&sid),
        "the redacted entry is carried forward unchanged"
    );
    land(&alice, "main", &c3).await;

    // --- A rebase needing the redacted content reports a `redacted` conflict. ---
    let x = TestCluster::commit(
        &alice,
        &c3,
        &[("app/secret.txt", b"rotated by x\n")],
        &[],
        5,
    )
    .await;
    let y = TestCluster::commit(
        &alice,
        &c3,
        &[("app/secret.txt", b"rotated by y\n")],
        &[],
        6,
    )
    .await;
    let e = alice
        .write()
        .rebase(v1::RebaseRequest {
            changeset: idb(&x),
            onto: idb(&y),
        })
        .await
        .unwrap_err();
    let err = error_from_status(&e);
    assert_eq!(err.code, ErrorCode::MergeConflict, "{err:?}");
    match &err.payload {
        ErrorPayload::MergeConflict(report) => {
            let conflict = report
                .conflicts
                .iter()
                .find(|c| c.path == b"app/secret.txt")
                .expect("the conflict names the path");
            assert_eq!(conflict.kind, ConflictKind::Redacted);
        }
        other => panic!("conflict payload expected: {other:?}"),
    }

    // --- Meta redaction: Log renders <redacted>, ids unchanged. ---
    let log = |from: ObjectId| {
        let alice = alice.clone();
        async move {
            alice
                .query()
                .log(v1::LogRequest {
                    from: idb(&from),
                    path: None,
                    first_parent_only: true,
                    limit: 10,
                    page_token: Vec::new(),
                })
                .await
                .unwrap()
                .into_inner()
        }
    };
    let entries = log(c2).await.entries;
    let meta2 = entries[0].meta.as_ref().unwrap();
    assert!(!meta2.redacted);
    let meta_id = ObjectId::from_bytes(&meta2.meta_id).unwrap();
    redact(&admin, &[meta_id], "personal data in message").await;
    let after = log(c2).await.entries;
    assert_eq!(after[0].id, idb(&c2), "changeset id unchanged");
    let view = after[0].meta.as_ref().unwrap();
    assert!(view.redacted, "Log renders the Meta as redacted: {view:?}");
    assert!(
        view.message.is_empty() && view.author.is_empty(),
        "no Meta content: {view:?}"
    );
    assert_eq!(view.meta_id, idb(&meta_id));
    assert!(
        !after[1].meta.as_ref().unwrap().redacted,
        "other Metas unaffected"
    );
    assert_eq!(changesets(&alice, &[c2]).await[0].status, OK);

    // --- Unredact from quarantine restores reads byte-identically. ---
    redact(&admin, &[kid], "mistake").await;
    assert_eq!(read_status(&alice, &kid).await.0, REDACTED);
    admin
        .admin()
        .unredact(v1::UnredactRequest {
            object_ids: vec![idb(&kid)],
        })
        .await
        .unwrap();
    assert_eq!(read_status(&alice, &kid).await, (OK, keep.clone()));

    // --- Purge: bytes gone from both keys; unredact after purge fails. ---
    let ctx = c.servers[0].ctx();
    admin
        .admin()
        .purge_now(v1::PurgeNowRequest {
            object_ids: vec![idb(&sid)],
            reason: "credential rotated".into(),
        })
        .await
        .unwrap();
    for area in [Area::Live, Area::Quarantine] {
        let key = ctx.objects.key(&sid, area);
        assert!(!c.storage.blobs.exists(&key).await.unwrap(), "{key} purged");
    }
    let row = c
        .storage
        .meta
        .get_objects(&[sid], Consistency::Strong)
        .await
        .unwrap()
        .pop()
        .flatten()
        .unwrap();
    assert_eq!(row.state, ObjectState::Purged);
    assert!(row.inline_data.is_none());
    assert_eq!(read_status(&alice, &sid).await.0, REDACTED);
    let e = admin
        .admin()
        .unredact(v1::UnredactRequest {
            object_ids: vec![idb(&sid)],
        })
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&e).code, ErrorCode::InvalidArgument);
    // Purged trees and changesets are untouched.
    assert_eq!(list_tree(&alice, &c3, "app").await, carried);

    // --- The audit log records every action with actor and reason. ---
    let audit = c.storage.meta.audit_log(1000).await.unwrap();
    for (action, target, reason) in [
        ("redact", sid.to_text(), "leaked credential"),
        ("redact", meta_id.to_text(), "personal data in message"),
        ("unredact", kid.to_text(), ""),
        ("purge", sid.to_text(), "credential rotated"),
    ] {
        let found = audit.iter().find(|a| {
            a.action == action
                && a.actor == "github:admin"
                && a.target.contains(&target)
                && (reason.is_empty() || a.reason == reason)
        });
        assert!(found.is_some(), "audit row {action} {target}: {audit:#?}");
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn redaction_runbook_through_servers_and_daemon() {
    run_runbook(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_redaction_runbook_through_servers_and_daemon() {
    if skip_external() {
        return;
    }
    run_runbook(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Daemon fail-closed.
// ---------------------------------------------------------------------------------------

async fn run_fail_closed(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let content = format!("fail closed {t}\n").into_bytes();
    let id = ObjectId::hash_blob(&content);
    TestCluster::commit(&alice, &root, &[("fc/a.txt", &content)], &[], 1).await;
    assert_eq!(read_status(&alice_d, &id).await, (OK, content.clone()));
    let d = c.daemon.as_ref().unwrap();
    // A warm hit while fresh.
    let s0 = d.handle.stats();
    assert_eq!(read_status(&alice_d, &id).await, (OK, content.clone()));
    assert_eq!(d.handle.stats().cache_hits, s0.cache_hits + 1);
    assert!(!d.handle.stats().fail_closed);

    d.handle.block_redaction_polling(true);
    tokio::time::sleep(Duration::from_millis(STALENESS_MS + POLL_MS + SLACK_MS)).await;
    eventually("fail closed", || d.handle.stats().fail_closed).await;
    let s1 = d.handle.stats();
    for _ in 0..3 {
        assert_eq!(
            read_status(&alice_d, &id).await,
            (OK, content.clone()),
            "forwarded read"
        );
    }
    let s2 = d.handle.stats();
    assert_eq!(
        s2.cache_hits, s1.cache_hits,
        "no read was served from cache while fail-closed"
    );
    assert!(
        s2.upstream_requests >= s1.upstream_requests + 3,
        "reads were forwarded"
    );

    d.handle.block_redaction_polling(false);
    eventually("polling resumes", || !d.handle.stats().fail_closed).await;
    let s3 = d.handle.stats();
    assert_eq!(read_status(&alice_d, &id).await, (OK, content));
    assert_eq!(
        d.handle.stats().cache_hits,
        s3.cache_hits + 1,
        "cache serving resumed"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_fails_closed_when_redactions_are_stale() {
    run_fail_closed(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_daemon_fails_closed_when_redactions_are_stale() {
    if skip_external() {
        return;
    }
    run_fail_closed(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Working copies.
// ---------------------------------------------------------------------------------------

async fn create_ref(c: &TestCluster, name: &str, target: &ObjectId) {
    let admin = c.client(Via::Server, &gh("admin")).await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: idb(target),
            reason: "redaction suite".into(),
        })
        .await
        .unwrap();
}

async fn run_working_copies(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let admin = c.client(Via::Server, &gh("admin")).await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let clean = format!("clean, soon redacted {t}\n").into_bytes();
    let dirty = format!("dirty, soon redacted {t}\n").into_bytes();
    let other = format!("unrelated {t}\n").into_bytes();
    let base = TestCluster::commit(
        &alice,
        &root,
        &[
            ("wc/clean.txt", &clean),
            ("wc/dirty.txt", &dirty),
            ("wc/other.txt", &other),
        ],
        &[],
        1,
    )
    .await;
    land(&alice, "main", &base).await;
    create_ref(&c, "red-server", &base).await;
    create_ref(&c, "red-daemon", &base).await;

    for via in Via::ALL {
        // Placeholders on checkout are clean; editing one commits new content (P28 scenario).
        let s = Scenario::new(c.client(via, &gh("alice")).await).unwrap();
        let r = RefName::parse(&format!("red-{}", via.as_str())).unwrap();
        wct::redacted_placeholder_commit(&s, &admin, &r)
            .await
            .unwrap_or_else(|e| panic!("{}: placeholder scenario: {e}", via.as_str()));
    }

    // Newly redacted clean files are replaced on the next mutating command; dirty files are
    // untouched and reported.
    let s = Scenario::new(c.client(Via::Daemon, &gh("alice")).await).unwrap();
    let mut wcs = Vec::new();
    for name in ["mutating", "refresh"] {
        let root = s.fresh_dir(name);
        let wc = WorkingCopy::checkout(
            s.client.clone(),
            &root,
            "main",
            Vec::new(),
            WcOptions::default(),
        )
        .await
        .unwrap();
        wcs.push((root, wc));
    }
    let dirty_p = RepoPath::parse(b"wc/dirty.txt").unwrap();
    let local_edit = format!("local edit {t}\n").into_bytes();
    for (root, _) in &wcs {
        std::fs::write(root.join("wc/dirty.txt"), &local_edit).unwrap();
    }
    let cid = ObjectId::hash_blob(&clean);
    let did = ObjectId::hash_blob(&dirty);
    let r = redact(&admin, &[cid, did], "working copy runbook").await;
    // Let the daemon learn of the redaction (SPEC 13.3).
    sleep_until_ms(r.effective_by_unix_ms.max(now_ms()) + (POLL_MS + SLACK_MS) as i64).await;
    let d = c.daemon.as_ref().unwrap();
    eventually("daemon epoch", || {
        d.handle.stats().redaction_epoch >= r.epoch
    })
    .await;

    // (a) A mutating command (goto to the current base) replaces the clean copy.
    let (root_a, wc_a) = &mut wcs[0];
    let base_text = wc_a.state().base.to_text();
    wc_a.goto(&base_text, false).await.unwrap();
    assert_eq!(
        std::fs::read(root_a.join("wc/clean.txt")).unwrap(),
        placeholder_bytes(&cid)
    );
    assert_eq!(
        std::fs::read(root_a.join("wc/dirty.txt")).unwrap(),
        local_edit,
        "dirty untouched"
    );
    assert_eq!(std::fs::read(root_a.join("wc/other.txt")).unwrap(), other);
    let st = wc_a.status().await.unwrap();
    assert_eq!(
        st.modified,
        vec![dirty_p.clone()],
        "the placeholder is clean: {st:?}"
    );
    // (b) The refresh reports the dirty file.
    let (root_b, wc_b) = &mut wcs[1];
    let reported = wc_b.refresh_redactions().await.unwrap();
    assert_eq!(reported, vec![dirty_p.clone()]);
    assert_eq!(
        std::fs::read(root_b.join("wc/clean.txt")).unwrap(),
        placeholder_bytes(&cid)
    );
    assert_eq!(
        std::fs::read(root_b.join("wc/dirty.txt")).unwrap(),
        local_edit
    );
    // Committing the dirty file creates new content normally.
    let committed = wc_b
        .commit_with_report(b"keep local edit\n", b"Alice <alice@example.com>", None)
        .await
        .unwrap();
    assert_eq!(committed.delta.len(), 1);
    assert_eq!(committed.delta[0].path, dirty_p);
    let st = wc_b.status().await.unwrap();
    assert!(st.is_clean(), "{st:?}");
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn working_copy_placeholders_and_replacement() {
    run_working_copies(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_working_copy_placeholders_and_replacement() {
    if skip_external() {
        return;
    }
    run_working_copies(Backend::External).await;
}
