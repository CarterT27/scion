//! Operations: SPEC 19.12 policy reload, 10.5 rate limits and load shedding, 15.3 health,
//! restart durability and SPEC 15.1 RPC log fields (P23).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

mod core_support;

use bytes::Bytes;
use core_support::{admin, alice, epochs, eventually, http_get, opts};
use scion_core::{ErrorCode, FaultAction, Principal};
use scion_proto::v1;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_store::MetaStore;
use std::sync::Arc;

const POLICY_V2: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob"]
admins = ["github:admin"]
security = ["github:alice"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn policy_reload_changes_epoch_trailer_and_invalid_keeps_old() {
    let srv = InProcessServer::start(opts()).await;
    let c = srv.client(Some(&alice())).await;
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let e1 = epochs(r.metadata()).unwrap().policy;
    assert_eq!(r.into_inner().groups, vec!["eng".to_owned()]);
    let metrics = srv.ctx().metrics.clone();
    assert!(metrics.render().contains(&format!("epoch=\"{e1}\"")));

    // A valid change swaps atomically: new epoch on the very next response.
    srv.set_policy(POLICY_V2).unwrap();
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let e2 = epochs(r.metadata()).unwrap().policy;
    assert_ne!(e1, e2);
    assert_eq!(e2, srv.ctx().policy.epoch());
    assert_eq!(
        r.into_inner().groups,
        vec!["eng".to_owned(), "security".to_owned()]
    );
    eventually("policy epoch metric", || {
        metrics.render().contains(&format!("epoch=\"{e2}\""))
    })
    .await;
    let info = c
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.policy_epoch, e2);

    // An invalid file keeps the last good policy; the server keeps serving.
    let err = srv
        .set_policy("version = 1\n[[paths]]\nprefix = \"/x\"\n")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(srv.ctx().policy.last_error().is_some());
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(epochs(r.metadata()).unwrap().policy, e2);
    let err = srv.set_policy("this is not toml [").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(epochs(r.metadata()).unwrap().policy, e2);
    assert!(metrics.render().contains(&format!("epoch=\"{e2}\"")));

    // Restoring the original file restores the original epoch (hash of the normalized policy).
    srv.set_policy(scion_server::testing::DEFAULT_ACL).unwrap();
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(epochs(r.metadata()).unwrap().policy, e1);
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_watcher_applies_file_changes_when_reload_is_set() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.policy.reload = true)),
        ..opts()
    })
    .await;
    let e1 = srv.ctx().policy.epoch();
    std::fs::write(&srv.policy_path, POLICY_V2).unwrap();
    let ctx = srv.ctx();
    eventually("watcher reload", || ctx.policy.epoch() != e1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn per_principal_rate_limit() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| {
            c.rate_limit.per_principal_rps = 1;
            c.rate_limit.burst = 2;
        })),
        ..opts()
    })
    .await;
    let a = srv.client(Some(&alice())).await;
    a.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    a.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let err = a.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    let e = scion_proto::error_from_status(&err);
    assert_eq!(e.code, ErrorCode::ResourceExhausted);
    assert!(e.retryable());
    assert!(epochs(err.metadata()).is_some());
    // Another principal has its own budget.
    let b = srv.client(Some(&admin())).await;
    b.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn load_shedding_when_no_capacity() {
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(|c| c.rate_limit.max_inflight = 0)),
        ..opts()
    })
    .await;
    let c = srv.client(None).await;
    let err = c
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    assert!(scion_proto::error_from_status(&err).retryable());
    assert!(epochs(err.metadata()).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoints_reflect_dependencies() {
    let srv = InProcessServer::start(opts()).await;
    let admin_addr = srv.admin_addr().unwrap();
    assert_eq!(http_get(admin_addr, "/healthz").await.0, 200);
    assert_eq!(http_get(admin_addr, "/readyz").await.0, 200);

    srv.storage.faults.arm(
        "meta.ping.before",
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let (code, body) = http_get(admin_addr, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    assert!(body.contains("metadata"), "{body}");
    assert_eq!(http_get(admin_addr, "/healthz").await.0, 200);
    srv.storage.faults.disarm_all();
    assert_eq!(http_get(admin_addr, "/readyz").await.0, 200);

    let mem = srv.mem.as_ref().unwrap();
    mem.blobs.set_down(true);
    let (code, body) = http_get(admin_addr, "/readyz").await;
    assert_eq!(code, 503, "{body}");
    mem.blobs.set_down(false);
    assert_eq!(http_get(admin_addr, "/readyz").await.0, 200);

    // Metrics are served on the same listener and move with traffic.
    let c = srv.client(Some(&alice())).await;
    c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let (code, text) = http_get(admin_addr, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        text.contains(
            "scion_rpc_duration_seconds_count{code=\"OK\",rpc=\"WhoAmI\",tier=\"server\"}"
        ),
        "{text}"
    );
    assert!(
        text.contains("scion_redaction_epoch{tier=\"server\"} 0"),
        "{text}"
    );
    assert!(text.contains("scion_policy_epoch_info"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_keeps_data_and_address() {
    let mut srv = InProcessServer::start(opts()).await;
    let ctx = srv.ctx();
    let (id, _) = ctx
        .objects
        .put_blob(Bytes::from_static(b"survives restart"))
        .await
        .unwrap();
    let main_before = ctx
        .storage
        .meta
        .get_ref(&scion_object::RefName::main())
        .await
        .unwrap()
        .unwrap();
    let addr = srv.addr;
    let token = srv.token_for(&alice());

    let c = srv.client(None).await.with_token(Some(token.clone()));
    c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    srv.kill().await;
    // Killed: the open connection is dropped and nothing answers.
    assert!(c.auth().who_am_i(v1::WhoAmIRequest {}).await.is_err());

    srv.restart().await;
    assert_eq!(srv.addr, addr);
    let ctx = srv.ctx();
    assert_eq!(
        ctx.objects.get(&id).await.unwrap(),
        &b"survives restart"[..]
    );
    // Bootstrap is idempotent: main is untouched.
    let main_after = ctx
        .storage
        .meta
        .get_ref(&scion_object::RefName::main())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(main_after.version, main_before.version);
    assert_eq!(main_after.target, main_before.target);
    // Tokens issued before the restart still work (same keys).
    let c = srv.client(None).await.with_token(Some(token));
    let me = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(me.into_inner().principal, "github:alice");
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_log_events_have_required_fields() {
    let capture = scion_obs::capture::LogCapture::install();
    let repo = format!("logrepo-{}", uuid::Uuid::new_v4().simple());
    let repo2 = repo.clone();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |c| c.server.repo = repo2.clone())),
        service_principals: vec![("ci".into(), "very-secret-value".into())],
        ..opts()
    })
    .await;
    let anon = srv.client(None).await;
    anon.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    let login = anon
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: "ci".into(),
                    service_secret: "very-secret-value".into(),
                },
            )),
        })
        .await
        .unwrap()
        .into_inner();
    anon.auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .unwrap_err();
    let c = srv.client(Some(&alice())).await;
    c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let _ = c
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: "main".into(),
            from_version: 0,
            limit: 1,
        })
        .await;

    let want = [
        ("GetServerInfo", "OK", None),
        ("Login", "OK", Some("service:ci")),
        ("WhoAmI", "UNAUTHENTICATED", None),
        ("WhoAmI", "OK", Some("github:alice")),
        ("GetRefLog", "", Some("github:alice")),
    ];
    let events = || -> Vec<serde_json::Value> {
        capture
            .events()
            .into_iter()
            .filter(|e| e["target"] == "scion::rpc" && e["repo"] == repo.as_str())
            .collect()
    };
    eventually("rpc log events", || events().len() >= want.len()).await;
    let evs = events();
    for e in &evs {
        for field in [
            "request_id",
            "rpc",
            "tier",
            "principal",
            "repo",
            "status",
            "duration_ms",
        ] {
            assert!(e.get(field).is_some(), "missing {field} in {e}");
        }
        assert_eq!(e["tier"], "server");
        assert!(!e["request_id"].as_str().unwrap().is_empty());
        assert!(e["duration_ms"].is_u64());
    }
    for (rpc, status, principal) in want {
        let found = evs.iter().any(|e| {
            e["rpc"] == rpc
                && (status.is_empty() || e["status"] == status)
                && match principal {
                    Some(p) => e["principal"] == p,
                    None => e["principal"] == "anonymous",
                }
        });
        assert!(
            found,
            "no event for {rpc}/{status}/{principal:?} in {evs:?}"
        );
    }
    // Neither the secret nor any token appears in the logs.
    let token = srv.token_for(&Principal::Service("ci".into()));
    assert!(
        capture
            .find_needles(&[
                b"very-secret-value",
                login.token.as_bytes(),
                token.as_bytes()
            ])
            .is_empty()
    );
}
