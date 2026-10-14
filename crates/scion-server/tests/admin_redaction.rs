//! SPEC 19.7 / 19.13 (server side) / 19.12 admin permissions for Redact, Unredact, PurgeNow
//! and ListRedactions, plus the background quarantine mover and purge scheduler (P25).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;

use object_support::*;
use scion_core::fault::{FaultAction, points};
use scion_core::{ErrorCode, ManualClock, ObjectId, subcode as sc};
use scion_proto::{ScionClient, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_store::{Area, ObjectState};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

async fn redact_rpc(
    c: &ScionClient,
    want: &[ObjectId],
    reason: &str,
    purge_after: Option<u64>,
) -> Result<tonic::Response<v1::RedactResponse>, tonic::Status> {
    c.admin()
        .redact(v1::RedactRequest {
            object_ids: ids(want),
            reason: reason.into(),
            purge_after_seconds: purge_after,
        })
        .await
}

async fn unredact_rpc(c: &ScionClient, want: &[ObjectId]) -> Result<u64, tonic::Status> {
    Ok(c.admin()
        .unredact(v1::UnredactRequest {
            object_ids: ids(want),
        })
        .await?
        .into_inner()
        .epoch)
}

async fn purge_rpc(c: &ScionClient, want: &[ObjectId], reason: &str) -> Result<(), tonic::Status> {
    c.admin()
        .purge_now(v1::PurgeNowRequest {
            object_ids: ids(want),
            reason: reason.into(),
        })
        .await
        .map(|_| ())
}

async fn list_rpc(
    c: &ScionClient,
    since: u64,
) -> Result<v1::ListRedactionsResponse, tonic::Status> {
    Ok(c.admin()
        .list_redactions(v1::ListRedactionsRequest { since_epoch: since })
        .await?
        .into_inner())
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test(flavor = "multi_thread")]
async fn redact_takes_effect_by_effective_by_and_unredact_restores_bytes() {
    let start = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let clock = ManualClock::new(start);
    let srv = InProcessServer::start(ServerTestOpts {
        clock: Some(clock.clone()),
        ..opts()
    })
    .await;
    let ctx = srv.ctx();
    let content = b"AWS_SECRET=hunter2\n".repeat(10);
    let cs = commit(&srv, &[("cfg/env", &content), ("cfg/other", b"keep me")]).await;
    let leaked = cs.blobs["cfg/env"];
    let a = srv.client(Some(&alice())).await;
    assert_eq!(read_ok(&a, &leaked).await, content);
    let ids_before = (
        cs.changeset,
        cs.root_tree,
        ctx.history
            .resolve_ref(&scion_object::RefName::main(), None)
            .await
            .unwrap(),
    );

    let adm = srv.client(Some(&admin())).await;
    let resp = redact_rpc(&adm, &[leaked], "leaked credential", None)
        .await
        .unwrap();
    let md = resp.metadata().clone();
    let r = resp.into_inner();
    assert_eq!(r.epoch, 1);
    let bound = srv.config().redaction.propagation_bound_ms as i64;
    assert_eq!(r.effective_by_unix_ms, unix_ms(start) + bound);
    assert_eq!(epochs(&md).unwrap().redaction, 1);

    // Served as redacted no later than effective_by (here: at once, the clock has not moved).
    let resp = a
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(&[leaked]),
            offset: None,
            length: None,
        })
        .await
        .unwrap();
    let mut s = resp.into_inner();
    let ch = s.message().await.unwrap().unwrap();
    assert_eq!(ch.status, v1::ItemStatus::Redacted as i32);
    assert!(ch.data.is_empty());
    assert!(s.message().await.unwrap().is_none());
    assert_eq!(
        epochs(&s.trailers().await.unwrap().unwrap())
            .unwrap()
            .redaction,
        1
    );
    assert!(unix_ms(ctx.clock.now()) <= r.effective_by_unix_ms);
    assert_eq!(has(&a, &[leaked]).await, vec![false]);
    let ca = a
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: srv.token_for(&alice()),
            object_ids: ids(&[leaked]),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ca.allowed, vec![false]);
    // The bytes moved to the quarantine key.
    let mem = srv.mem.as_ref().unwrap();
    assert!(
        mem.blobs
            .raw(&ctx.objects.key(&leaked, Area::Live))
            .is_none()
    );
    assert!(
        mem.blobs
            .raw(&ctx.objects.key(&leaked, Area::Quarantine))
            .is_some()
    );

    // No tree, changeset or ref id changed; the rest of the tree still reads.
    let ids_after = (
        cs.changeset,
        cs.root_tree,
        ctx.history
            .resolve_ref(&scion_object::RefName::main(), None)
            .await
            .unwrap(),
    );
    assert_eq!(ids_before, ids_after);
    assert_eq!(status_of(&a, &cs.root_tree).await, v1::ItemStatus::Ok);
    assert_eq!(read_ok(&a, &cs.blobs["cfg/other"]).await, b"keep me");

    // Re-introducing the content fails REDACTED.
    let (_, up) = upload_blob(&a, &content).await;
    assert_eq!(up.status, v1::ItemStatus::Redacted as i32);
    assert_eq!(up.error_code, "REDACTED");

    // ListRedactions: entries and the current epoch.
    let l = list_rpc(&a, 0).await.unwrap();
    assert_eq!(l.current_epoch, 1);
    assert_eq!(l.entries.len(), 1);
    assert_eq!(l.entries[0].object_id, leaked.as_bytes().to_vec());
    assert_eq!(l.entries[0].state, "quarantined");
    assert_eq!(l.entries[0].epoch, 1);
    assert_eq!(
        l.entries[0].reason, "",
        "reasons are for redaction admins only"
    );
    let l = list_rpc(&adm, 0).await.unwrap();
    assert_eq!(l.entries[0].reason, "leaked credential");
    assert!(list_rpc(&a, 1).await.unwrap().entries.is_empty());

    // Unredact restores reads byte-identically.
    let e = unredact_rpc(&adm, &[leaked]).await.unwrap();
    assert_eq!(e, 2);
    assert_eq!(read_ok(&a, &leaked).await, content);
    assert_eq!(has(&a, &[leaked]).await, vec![true]);
    let l = list_rpc(&a, 0).await.unwrap();
    assert_eq!((l.entries.len(), l.current_epoch), (0, 2));
    // Unredacting a live object is not_quarantined.
    let err = unredact_rpc(&adm, &[leaked]).await.unwrap_err();
    assert_eq!(subcode(&err).as_deref(), Some(sc::NOT_QUARANTINED));

    // The audit log records every action with actor and reason.
    let audit = ctx.storage.meta.audit_log(100).await.unwrap();
    let redact = audit
        .iter()
        .find(|r| r.action == "redact")
        .expect("redact audited");
    assert_eq!(redact.actor, "github:admin");
    assert_eq!(redact.reason, "leaked credential");
    assert!(redact.target.contains(&leaked.to_text()));
    let un = audit
        .iter()
        .find(|r| r.action == "unredact")
        .expect("unredact audited");
    assert_eq!(un.actor, "github:admin");
}

#[tokio::test(flavor = "multi_thread")]
async fn redaction_set_refresher_propagates_within_the_bound() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.redaction.propagation_bound_ms = 200)),
        ..opts()
    })
    .await;
    let ctx = srv.ctx();
    let cs = commit(&srv, &[("a.txt", b"redact me elsewhere")]).await;
    let id = cs.blobs["a.txt"];
    // A redaction committed by another server (straight through the Redactor, without this
    // server's immediate refresh) still reaches this server's set within the bound.
    let t0 = Instant::now();
    let out = ctx
        .redactor
        .redact(&[id], "other server", &admin(), None)
        .await
        .unwrap();
    while !ctx.redactions.contains(&id) {
        assert!(t0.elapsed() < Duration::from_secs(5), "never propagated");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        t0.elapsed() <= Duration::from_millis(200 + 800),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(ctx.redactions.epoch(), out.epoch);
    assert_eq!(
        status_of(&srv.client(Some(&alice())).await, &id).await,
        v1::ItemStatus::Redacted
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_now_is_irreversible_and_audited() {
    let srv = start().await;
    let ctx = srv.ctx();
    let cs = commit(&srv, &[("p.txt", b"purge me"), ("live.txt", b"live")]).await;
    let id = cs.blobs["p.txt"];
    let live = cs.blobs["live.txt"];
    let adm = srv.client(Some(&admin())).await;
    // Purging a live object is not_quarantined; an unknown one NOT_FOUND.
    let err = purge_rpc(&adm, &[live], "x").await.unwrap_err();
    assert_eq!(subcode(&err).as_deref(), Some(sc::NOT_QUARANTINED));
    let err = purge_rpc(&adm, &[ObjectId::hash_blob(b"nope")], "x")
        .await
        .unwrap_err();
    assert_eq!(code(&err), ErrorCode::NotFound);

    redact_rpc(&adm, &[id], "secret", None).await.unwrap();
    purge_rpc(&adm, &[id], "confirmed leak").await.unwrap();
    // Idempotent for already purged ids; an empty reason uses the default.
    purge_rpc(&adm, &[id], "").await.unwrap();
    let mem = srv.mem.as_ref().unwrap();
    assert!(mem.blobs.raw(&ctx.objects.key(&id, Area::Live)).is_none());
    assert!(
        mem.blobs
            .raw(&ctx.objects.key(&id, Area::Quarantine))
            .is_none()
    );
    let row = ctx.objects.stat(&id).await.unwrap().unwrap();
    assert_eq!(row.state, ObjectState::Purged);
    let a = srv.client(Some(&alice())).await;
    assert_eq!(status_of(&a, &id).await, v1::ItemStatus::Redacted);
    let l = list_rpc(&a, 0).await.unwrap();
    assert_eq!(l.entries[0].state, "purged");
    // Unredact after purge fails.
    let err = unredact_rpc(&adm, &[id]).await.unwrap_err();
    assert_eq!(subcode(&err).as_deref(), Some(sc::NOT_QUARANTINED));
    let audit = ctx.storage.meta.audit_log(100).await.unwrap();
    let purge = audit
        .iter()
        .find(|r| r.action == "purge")
        .expect("purge audited");
    assert_eq!(purge.actor, "github:admin");
    assert_eq!(purge.reason, "confirmed leak");
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_rpcs_require_their_permissions_and_a_token() {
    let srv = start().await;
    let cs = commit(&srv, &[("x.txt", b"x"), ("y.txt", b"y")]).await;
    let x = cs.blobs["x.txt"];
    let y = cs.blobs["y.txt"];
    let denied = |r: tonic::Status| {
        assert_eq!(r.code(), tonic::Code::PermissionDenied, "{r:?}");
        assert!(epochs(r.metadata()).is_some());
    };
    for p in [alice(), carol(), dave()] {
        let c = srv.client(Some(&p)).await;
        denied(redact_rpc(&c, &[x], "r", None).await.unwrap_err());
        denied(unredact_rpc(&c, &[x]).await.unwrap_err());
        denied(purge_rpc(&c, &[x], "r").await.unwrap_err());
        // ListRedactions needs a valid token only.
        list_rpc(&c, 0).await.unwrap();
    }
    // admin.redact without admin.purge.
    let r = srv.client(Some(&redactor())).await;
    redact_rpc(&r, &[x, y], "r", None).await.unwrap();
    denied(purge_rpc(&r, &[x], "r").await.unwrap_err());
    unredact_rpc(&r, &[y]).await.unwrap();
    // The audit actor is the caller.
    let audit = srv.ctx().storage.meta.audit_log(100).await.unwrap();
    assert!(
        audit
            .iter()
            .any(|a| a.action == "redact" && a.actor == "github:redactor")
    );
    // A denied attempt changes nothing.
    assert_eq!(srv.ctx().redactions.epoch(), 2);

    let anon = srv.client(None).await;
    for e in [
        redact_rpc(&anon, &[x], "r", None).await.unwrap_err(),
        anon.admin()
            .unredact(v1::UnredactRequest {
                object_ids: ids(&[x]),
            })
            .await
            .unwrap_err(),
        anon.admin()
            .purge_now(v1::PurgeNowRequest {
                object_ids: ids(&[x]),
                reason: "r".into(),
            })
            .await
            .unwrap_err(),
        list_rpc(&anon, 0).await.unwrap_err(),
    ] {
        assert_eq!(e.code(), tonic::Code::Unauthenticated);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redact_validation_errors() {
    let srv = start().await;
    let cs = commit(&srv, &[("d/f.txt", b"f")]).await;
    let adm = srv.client(Some(&admin())).await;
    let cases: Vec<(Vec<Vec<u8>>, &str, ErrorCode, Option<&str>)> = vec![
        (
            ids(&[cs.root_tree]),
            "r",
            ErrorCode::InvalidArgument,
            Some(sc::NOT_REDACTABLE),
        ),
        (
            ids(&[cs.changeset]),
            "r",
            ErrorCode::InvalidArgument,
            Some(sc::NOT_REDACTABLE),
        ),
        (
            ids(&[ObjectId::hash_blob(b"unknown")]),
            "r",
            ErrorCode::NotFound,
            None,
        ),
        (
            Vec::new(),
            "r",
            ErrorCode::InvalidArgument,
            Some(sc::MISSING_FIELD),
        ),
        (
            ids(&[cs.blobs["d/f.txt"]]),
            "  ",
            ErrorCode::InvalidArgument,
            Some(sc::MISSING_FIELD),
        ),
        (
            vec![vec![9; 5]],
            "r",
            ErrorCode::InvalidArgument,
            Some(sc::BAD_ID_TEXT),
        ),
    ];
    for (want, reason, c, sub) in cases {
        let err = adm
            .admin()
            .redact(v1::RedactRequest {
                object_ids: want,
                reason: reason.into(),
                purge_after_seconds: None,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&err), c);
        if let Some(s) = sub {
            assert_eq!(subcode(&err).as_deref(), Some(s));
        }
    }
    for e in [
        unredact_rpc(&adm, &[]).await.unwrap_err(),
        purge_rpc(&adm, &[], "r").await.unwrap_err(),
    ] {
        assert_eq!(subcode(&e).as_deref(), Some(sc::MISSING_FIELD));
    }
    let err = unredact_rpc(&adm, &[ObjectId::hash_blob(b"unknown")])
        .await
        .unwrap_err();
    assert_eq!(subcode(&err).as_deref(), Some(sc::NOT_QUARANTINED));
    // Metadata outage: UNAVAILABLE.
    srv.storage.faults.arm(
        &points::meta_before("list_redactions"),
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let err = list_rpc(&adm, 0).await.unwrap_err();
    assert_eq!(code(&err), ErrorCode::Unavailable);
    srv.storage.faults.disarm_all();
    // A Meta is redactable; GetObjects then reports it redacted.
    redact_rpc(&adm, &[cs.meta], "author email", None)
        .await
        .unwrap();
    assert_eq!(
        status_of(&srv.client(Some(&alice())).await, &cs.meta).await,
        v1::ItemStatus::Redacted
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn background_jobs_finish_moves_and_run_scheduled_purges() {
    let clock = ManualClock::new(SystemTime::now());
    let mut srv = InProcessServer::start(ServerTestOpts {
        clock: Some(clock.clone()),
        ..opts()
    })
    .await;
    let cs = commit(&srv, &[("m.txt", b"move me"), ("p.txt", b"purge me later")]).await;
    let (m, p) = (cs.blobs["m.txt"], cs.blobs["p.txt"]);
    let adm = srv.client(Some(&admin())).await;
    // The byte move after the commit fails once (crash between copy and delete).
    srv.storage.faults.arm(
        points::QUARANTINE_AFTER_COPY,
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    redact_rpc(&adm, &[m], "m", None).await.unwrap();
    let ctx = srv.ctx();
    let live_m = ctx.objects.key(&m, Area::Live);
    assert!(srv.mem.as_ref().unwrap().blobs.raw(&live_m).is_some());
    redact_rpc(&adm, &[p], "p", Some(10)).await.unwrap();
    clock.advance(Duration::from_secs(11));

    // A restarted server's background jobs run at once: the mover finishes the move and the
    // scheduler purges the due object.
    srv.restart().await;
    let ctx = srv.ctx();
    let t0 = Instant::now();
    loop {
        let moved = srv.mem.as_ref().unwrap().blobs.raw(&live_m).is_none();
        let purged = ctx.objects.stat(&p).await.unwrap().unwrap().state == ObjectState::Purged;
        if moved && purged {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "background jobs did not run"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let audit = ctx.storage.meta.audit_log(100).await.unwrap();
    let purge = audit.iter().find(|r| r.action == "purge").unwrap();
    assert_eq!(purge.actor, scion_redaction::SCHEDULER_ACTOR);
    assert_eq!(purge.reason, scion_redaction::SCHEDULED_PURGE_REASON);
    // The moved object is still quarantined and can be restored.
    unredact_rpc(&srv.client(Some(&admin())).await, &[m])
        .await
        .unwrap();
    assert_eq!(
        read_ok(&srv.client(Some(&alice())).await, &m).await,
        b"move me"
    );
}
