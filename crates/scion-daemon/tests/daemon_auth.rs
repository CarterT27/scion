//! SPEC 19.11 (daemon token rejection) and SPEC 19.12 (daemon isolation of decisions across
//! principals; policy epoch changes drop decisions).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod daemon_support;

use daemon_support::*;
use scion_core::Principal;
use scion_proto::{ScionClient, v1};
use scion_testkit::upstream_methods;
use std::time::Duration;

async fn client_with(h: &Harness, token: &str) -> ScionClient {
    ScionClient::connect(&h.daemon().endpoint(), None, Some(token.to_owned()))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_rejects_expired_malformed_wrong_key_and_tampered_tokens() {
    let h = Harness::start(Opts::default()).await;
    let (id, _) = h.blob(1, 100, false);
    h.up.set_ref("main", id, 1);
    // Warm the cache with a valid token so rejection is not an upstream decision.
    let good = h.via_daemon(ALICE).await;
    get_one(&good, id).await.unwrap();
    let alice = Principal::parse(ALICE).unwrap();
    let expired = h
        .keyring
        .issue(
            &alice,
            Duration::from_secs(60),
            h.now() - Duration::from_secs(3600),
        )
        .unwrap()
        .0;
    let wrong_key = scion_auth::Keyring::generate_for_tests(1)
        .issue(&alice, Duration::from_secs(3600), h.now())
        .unwrap()
        .0;
    let valid = h.token(ALICE);
    let tampered = {
        // Flip one character inside the signed payload.
        let mut b = valid.clone().into_bytes();
        let i = "v4.public.".len() + 5;
        b[i] = if b[i] == b'A' { b'B' } else { b'A' };
        String::from_utf8(b).unwrap()
    };
    let before = h.upstream_get_objects();
    for (what, tok) in [
        ("expired", expired),
        ("malformed", "not-a-token".to_owned()),
        ("wrong key", wrong_key),
        ("tampered", tampered),
    ] {
        let c = client_with(&h, &tok).await;
        let e = get_one(&c, id).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated, "{what}: GetObjects");
        let e = c
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "main".into(),
                at_version: None,
            })
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unauthenticated, "{what}: ResolveRef");
    }
    // Missing token.
    let anon = ScionClient::connect(&h.daemon().endpoint(), None, None)
        .await
        .unwrap();
    let e = get_one(&anon, id).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
    assert_eq!(h.upstream_get_objects(), before, "rejected locally");
    // The valid token still works.
    assert_eq!(get_one(&good, id).await.unwrap().status, OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn restricted_object_cached_for_a_is_not_served_to_b_without_bs_decision() {
    let h = Harness::start(Opts::default()).await;
    let (r, rdata) = h.blob(1, 4000, true);
    h.up.deny(BOB, r);
    let a = h.via_daemon(ALICE).await;
    let b = h.via_daemon(BOB).await;
    let item = get_one(&a, r).await.unwrap();
    assert_eq!(item.status, OK);
    assert_eq!(item.access_class, v1::AccessClass::Restricted as i32);
    assert!(h.daemon().cache().contains(&r));
    // A again: served from cache with A's own cached decision (no upstream call).
    let calls = h.up.requests().len();
    assert_eq!(get_one(&a, r).await.unwrap().content, rdata.to_vec());
    assert_eq!(h.up.requests().len(), calls);
    // B: the daemon asks upstream for B's own decision and serves B's status.
    let item = get_one(&b, r).await.unwrap();
    assert_eq!(item.status, DENIED);
    assert!(item.content.is_empty());
    let checks = h.up.requests_for(upstream_methods::CHECK_ACCESS);
    assert!(
        checks.iter().any(|c| c.principal.as_deref() == Some(BOB)),
        "B's own CheckAccess"
    );
    // Asking again never reuses A's positive decision.
    assert_eq!(get_one(&b, r).await.unwrap().status, DENIED);
    // A third principal allowed upstream gets the bytes only after its own positive decision.
    let carol = "github:carol";
    let c = h.via_daemon(carol).await;
    let n = h.upstream_calls(upstream_methods::CHECK_ACCESS);
    assert_eq!(get_one(&c, r).await.unwrap().content, rdata.to_vec());
    assert_eq!(h.upstream_calls(upstream_methods::CHECK_ACCESS), n + 1);
    let last = h.up.requests_for(upstream_methods::CHECK_ACCESS);
    assert_eq!(last.last().unwrap().principal.as_deref(), Some(carol));
}

#[tokio::test(flavor = "multi_thread")]
async fn root_class_objects_need_a_confirmed_root_reader() {
    let h = Harness::start(Opts::default()).await;
    let objs: Vec<_> = (0..4).map(|i| h.blob(10 + i, 100, false)).collect();
    let a = h.via_daemon(ALICE).await;
    // A fetches everything (cache warm, A's per-object decisions recorded).
    for (id, _) in &objs {
        get_one(&a, *id).await.unwrap();
    }
    // B: first cached root object needs B's CheckAccess; that positive root-class answer
    // confirms B as a root reader, so the other root objects need no further call.
    let b = h.via_daemon(BOB).await;
    let n = h.upstream_calls(upstream_methods::CHECK_ACCESS);
    get_one(&b, objs[0].0).await.unwrap();
    assert_eq!(h.upstream_calls(upstream_methods::CHECK_ACCESS), n + 1);
    for (id, data) in &objs[1..] {
        assert_eq!(get_one(&b, *id).await.unwrap().content, data.to_vec());
    }
    assert_eq!(h.upstream_calls(upstream_methods::CHECK_ACCESS), n + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_epoch_change_in_a_trailer_drops_decisions() {
    let h = Harness::start(Opts::default()).await;
    h.up.set_epochs(1, 0);
    let (r, rdata) = h.blob(1, 4000, true);
    let a = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&a, r).await.unwrap().content, rdata.to_vec());
    let n = h.upstream_calls(upstream_methods::CHECK_ACCESS);
    assert_eq!(get_one(&a, r).await.unwrap().status, OK);
    assert_eq!(
        h.upstream_calls(upstream_methods::CHECK_ACCESS),
        n,
        "decision cached"
    );
    // The policy changes upstream: A loses access. A forwarded call carries the new epoch.
    h.up.deny(ALICE, r);
    h.up.set_epochs(2, 0);
    a.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(h.daemon().stats().policy_epoch, 2);
    let item = get_one(&a, r).await.unwrap();
    assert_eq!(item.status, DENIED, "the dropped decision is re-checked");
    assert!(h.upstream_calls(upstream_methods::CHECK_ACCESS) > n);
}

#[tokio::test(flavor = "multi_thread")]
async fn decisions_expire_after_access_cache_ttl() {
    let h = Harness::start(Opts {
        manual_clock: true,
        tweak: Box::new(|c| c.access_cache_ttl_ms = 1000),
        ..Default::default()
    })
    .await;
    let clock = h.clock.clone().unwrap();
    let (r, _) = h.blob(1, 100, true);
    let a = h.via_daemon(ALICE).await;
    get_one(&a, r).await.unwrap();
    let n = h.upstream_calls(upstream_methods::CHECK_ACCESS);
    get_one(&a, r).await.unwrap();
    assert_eq!(h.upstream_calls(upstream_methods::CHECK_ACCESS), n);
    clock.advance(Duration::from_millis(1001));
    get_one(&a, r).await.unwrap();
    assert_eq!(h.upstream_calls(upstream_methods::CHECK_ACCESS), n + 1);
}

/// SPEC 10.4 through coalescing: B's request joins A's in-flight upstream fetch of a missing
/// restricted object; B never receives the bytes fetched with A's token without B's own
/// positive decision, and gets exactly B's upstream status.
#[tokio::test(flavor = "multi_thread")]
async fn coalesced_fetch_for_a_is_not_served_to_b_without_bs_decision() {
    let h = Harness::start(Opts {
        gate: true,
        ..Default::default()
    })
    .await;
    let (warm, _) = h.blob(9, 10, false);
    let (r, rdata) = h.blob(1, 5000, true);
    h.up.deny(BOB, r);
    let a = h.via_daemon(ALICE).await;
    let b = h.via_daemon(BOB).await;
    // Warm keys, tokens and the redaction sync for both principals.
    get_one(&a, warm).await.unwrap();
    get_one(&b, warm).await.unwrap();
    let gate = h.gate.clone().unwrap();
    gate.set_open(false);
    let ta = tokio::spawn({
        let a = a.clone();
        async move { get_one(&a, r).await }
    });
    assert!(eventually(Duration::from_secs(5), || gate.waiting() == 1).await);
    let tb = tokio::spawn({
        let b = b.clone();
        async move { get_one(&b, r).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gate.waiting(), 1, "B joined A's fetch instead of fetching");
    gate.set_open(true);
    let ia = ta.await.unwrap().unwrap();
    assert_eq!((ia.status, ia.content), (OK, rdata.to_vec()));
    let ib = tb.await.unwrap().unwrap();
    assert_eq!(ib.status, DENIED);
    assert!(ib.content.is_empty());
    let checks = h.up.requests_for(upstream_methods::CHECK_ACCESS);
    assert!(checks.iter().any(|c| c.principal.as_deref() == Some(BOB)));
}
