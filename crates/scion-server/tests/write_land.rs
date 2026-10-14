//! SPEC 19.5 / 19.7 / 19.12 / 15.2 via RPC: Land (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod write_support;

use scion_core::{ErrorCode, ErrorPayload, FaultAction, ObjectId, subcode};
use scion_object::RefName;
use scion_proto::{ScionClient, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;
use std::time::{Duration, Instant};
use write_support::*;

async fn land(
    c: &ScionClient,
    r: &str,
    head: &ObjectId,
    allow_empty: bool,
) -> Result<v1::LandResponse, tonic::Status> {
    c.write()
        .land(v1::LandRequest {
            r#ref: r.into(),
            head: head.as_bytes().to_vec(),
            allow_empty,
        })
        .await
        .map(|r| r.into_inner())
}

fn landed_ids(resp: &v1::LandResponse) -> Vec<(Vec<u8>, ObjectId, ObjectId)> {
    resp.landed
        .iter()
        .map(|l| (l.change_id.clone(), id(&l.source), id(&l.result)))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn land_happy_path_and_ref_log() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let metrics = srv.ctx().metrics.clone();
    let committed_before = metrics
        .land_attempts
        .with_label_values(&["committed"])
        .get();
    let durations_before = metrics.land_duration.get_sample_count();

    // A change on the tip lands as itself (rebase fast path).
    let c1 = commit(&srv, &c, &r, &[("one", b"1\n")], 1).await;
    let resp = land(&c, "main", &c1, false).await.unwrap();
    assert_eq!(id(&resp.target), c1);
    assert_eq!(resp.version, 2);
    assert_eq!(landed_ids(&resp), vec![(cid(1), c1, c1)]);

    // A two-member stack based on the old tip is rebased onto the new tip.
    let s1 = commit(&srv, &c, &r, &[("two", b"2\n")], 2).await;
    let s2 = commit(&srv, &c, &s1, &[("three", b"3\n")], 3).await;
    let resp = land(&c, "main", &s2, false).await.unwrap();
    assert_eq!(resp.version, 3);
    let landed = landed_ids(&resp);
    assert_eq!(landed.len(), 2);
    assert_eq!(landed[0].0, cid(2));
    assert_eq!(landed[0].1, s1);
    assert_eq!(landed[1].1, s2);
    assert_eq!(id(&resp.target), landed[1].2);
    let tip = srv
        .ctx()
        .history
        .changeset(&id(&resp.target))
        .await
        .unwrap();
    assert_eq!(tip.changeset.parents, vec![landed[0].2]);
    let h = &srv.ctx().history;
    for p in ["one", "two", "three"] {
        let path = scion_object::RepoPath::parse(p.as_bytes()).unwrap();
        assert!(
            h.lookup(&tip.changeset.root_tree, &path)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(ref_state(&srv, "main").await, (id(&resp.target), 3));

    // The ref log records both lands with their landed rows.
    let log = c
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: "main".into(),
            from_version: 2,
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner()
        .entries;
    assert_eq!(
        log.iter().map(|e| e.version).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(log[0].old_target, r.as_bytes().to_vec());
    assert_eq!(log[0].new_target, c1.as_bytes().to_vec());
    assert_eq!(log[1].landed.len(), 2);
    assert_eq!(log[1].principal, "github:alice");

    // The land metrics moved (SPEC 15.2).
    assert_eq!(
        metrics
            .land_attempts
            .with_label_values(&["committed"])
            .get(),
        committed_before + 2
    );
    assert_eq!(
        metrics.land_duration.get_sample_count(),
        durations_before + 2
    );
}

/// SPEC 19.5 idempotency: the CAS commits but the response is lost; the retry returns the
/// recorded result and does not create a second ref version.
#[tokio::test(flavor = "multi_thread")]
async fn land_retry_after_lost_response_returns_recorded_result() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let first = commit(&srv, &c, &r, &[("first", b"f\n")], 1).await;
    land(&c, "main", &first, false).await.unwrap();
    let mine = commit(&srv, &c, &r, &[("mine", b"m\n")], 2).await;

    srv.storage
        .faults
        .arm("meta.cas_ref.after", FaultAction::FailAfter, Some(1));
    let e = land(&c, "main", &mine, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::Unavailable, Some(subcode::INJECTED));
    assert!(got.retryable());
    srv.storage.faults.disarm_all();
    let (tip, version) = ref_state(&srv, "main").await;
    assert_eq!(version, 3, "the CAS committed before the failure");

    let idempotent_before = srv
        .ctx()
        .metrics
        .land_attempts
        .with_label_values(&["idempotent"])
        .get();
    let retry = land(&c, "main", &mine, false).await.unwrap();
    assert_eq!(id(&retry.target), tip);
    assert_eq!(retry.version, 3);
    assert_eq!(landed_ids(&retry), vec![(cid(2), mine, tip)]);
    assert_eq!(ref_state(&srv, "main").await, (tip, 3));
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 100)
        .await
        .unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(
        srv.ctx()
            .metrics
            .land_attempts
            .with_label_values(&["idempotent"])
            .get(),
        idempotent_before + 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn amended_change_gives_already_landed_payload() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let orig = commit(&srv, &c, &r, &[("f", b"v1\n")], 7).await;
    let landed = land(&c, "main", &orig, false).await.unwrap();
    let amended = commit(&srv, &c, &r, &[("f", b"v2\n")], 7).await;
    let e = land(&c, "main", &amended, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::AlreadyLanded, None);
    assert!(!got.retryable());
    assert_eq!(
        got.payload,
        ErrorPayload::AlreadyLanded {
            source: orig,
            result: id(&landed.target),
            version: landed.version,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn land_conflict_empty_change_and_allow_empty() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let a = commit(&srv, &c, &r, &[("f", b"a\n"), ("same", b"s\n")], 1).await;
    land(&c, "main", &a, false).await.unwrap();
    let (tip, v) = ref_state(&srv, "main").await;

    // MERGE_CONFLICT (add/add on f), with clean entries.
    let conflicting = commit(&srv, &c, &r, &[("f", b"b\n"), ("g", b"g\n")], 2).await;
    let e = land(&c, "main", &conflicting, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::MergeConflict, None);
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected MergeConflict payload");
    };
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.clean_entries.len(), 1);

    // EMPTY_CHANGE: the content is already on the ref.
    let dup = commit(&srv, &c, &r, &[("same", b"s\n")], 3).await;
    let e = land(&c, "main", &dup, false).await.unwrap_err();
    expect_err(&e, ErrorCode::EmptyChange, None);
    assert_eq!(ref_state(&srv, "main").await, (tip, v));

    // allow_empty: recorded with result = the current tip (PLAN 6.14).
    let resp = land(&c, "main", &dup, true).await.unwrap();
    assert_eq!(landed_ids(&resp), vec![(cid(3), dup, tip)]);
    assert_eq!(id(&resp.target), tip);
    // A retry is idempotent.
    let again = land(&c, "main", &dup, true).await.unwrap();
    assert_eq!(again.version, resp.version);
    assert_eq!(landed_ids(&again), landed_ids(&resp));
}

#[tokio::test(flavor = "multi_thread")]
async fn land_enforces_ref_update_and_path_write() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let admin_c = srv.client(Some(&admin())).await;
    let alice_c = srv.client(Some(&alice())).await;

    // Write on every delta path of every rebased member.
    let secret = commit(&srv, &admin_c, &r, &[("secrets/k", b"k\n")], 1).await;
    let e = land(&alice_c, "main", &secret, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert_eq!(ref_state(&srv, "main").await, (r, 1));
    land(&admin_c, "main", &secret, false).await.unwrap();

    // `update` on the ref: release/* is admins-only.
    admin_c
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "release/1".into(),
            target: r.as_bytes().to_vec(),
            reason: "branch cut".into(),
        })
        .await
        .unwrap();
    let pubc = commit(&srv, &alice_c, &r, &[("pub", b"p\n")], 2).await;
    let e = land(&alice_c, "release/1", &pubc, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    land(&admin_c, "release/1", &pubc, false).await.unwrap();

    // A reader without update is denied on main.
    let reader = srv.client(Some(&gh("reader"))).await;
    let e = land(&reader, "main", &pubc, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
}

/// SPEC 8.4: the policy is re-read on each land; a revoked grant applies to the next land.
#[tokio::test(flavor = "multi_thread")]
async fn land_uses_the_live_policy() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    srv.set_policy(&RESTRICTED_ACL.replace(
        "[refs.\"main\"]\nupdate = [\"group:eng\"]",
        "[refs.\"main\"]\nupdate = [\"group:admins\"]",
    ))
    .unwrap();
    let e = land(&c, "main", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    srv.set_policy(RESTRICTED_ACL).unwrap();
    land(&c, "main", &x, false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn land_argument_errors() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    let e = land(&c, "", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = land(&c, "bad..name", &x, false).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
    );
    let e = c
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: Vec::new(),
            allow_empty: false,
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = land(&c, "no-such-ref", &x, false).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    // A stack containing a merge changeset.
    let y = commit(&srv, &c, &r, &[("y", b"y\n")], 2).await;
    let m = c
        .write()
        .merge(v1::MergeRequest {
            a: x.as_bytes().to_vec(),
            b: y.as_bytes().to_vec(),
            meta_fields: Some(meta("merge")),
            change_id: cid(3),
        })
        .await
        .unwrap()
        .into_inner();
    let e = land(&c, "main", &id(&m.changeset_id), false)
        .await
        .unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::MERGE_IN_STACK),
    );
}

/// Concurrent non-overlapping lands through the RPC all land (lost CAS races retry); the ref
/// log has one entry per land and no gaps (SPEC 19.5, scaled down; the N = 10/100/1000 suite
/// is P30's).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_lands_all_succeed() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let n = 16u32;
    let mut heads = Vec::new();
    for i in 0..n {
        let path = format!("file{i}");
        heads.push(commit(&srv, &c, &r, &[(path.as_str(), b"content\n")], 100 + i).await);
    }
    let mut tasks = Vec::new();
    for h in heads {
        let c = c.clone();
        tasks.push(tokio::spawn(
            async move { land(&c, "main", &h, false).await },
        ));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let (_, v) = ref_state(&srv, "main").await;
    assert_eq!(v, 1 + u64::from(n));
    let log = srv
        .ctx()
        .history
        .ref_log(&RefName::main(), 0, 1000)
        .await
        .unwrap();
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(versions, (1..=1 + u64::from(n)).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn land_rpc_event_has_land_fields() {
    let capture = scion_obs::capture::LogCapture::install();
    let repo = format!("landlog-{}", uuid::Uuid::new_v4().simple());
    let repo2 = repo.clone();
    let base = opts();
    let base_tweak = base.tweak.clone().unwrap();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |cfg| {
            base_tweak(cfg);
            cfg.server.repo = repo2.clone();
        })),
        ..base
    })
    .await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 1).await;
    let resp = land(&c, "main", &x, false).await.unwrap();
    let find = || {
        capture.events().into_iter().find(|e| {
            e["target"] == "scion::rpc" && e["repo"] == repo.as_str() && e["rpc"] == "Land"
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let ev = loop {
        if let Some(ev) = find() {
            break ev;
        }
        assert!(Instant::now() < deadline, "no Land event");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(ev["status"], "OK");
    assert_eq!(ev["ref"], "main");
    assert_eq!(ev["ref_version"], resp.version);
    assert_eq!(ev["land_attempt"], 1);
    assert_eq!(ev["changeset_id"], id(&resp.target).to_text());
    assert_eq!(ev["principal"], "github:alice");
}
