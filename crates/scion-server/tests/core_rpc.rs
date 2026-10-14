//! SPEC 19.7 server core: unauthenticated RPCs, the bearer interceptor on every protected RPC,
//! and the three trailers on success, error and streaming responses (P23).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

mod core_support;

use core_support::{alice, descriptor, epochs, opts, raw_call};
use scion_core::Tier;
use scion_proto::v1;
use scion_server::testing::{InProcessServer, ServerTestOpts};

#[tokio::test(flavor = "multi_thread")]
async fn get_server_info_and_login_need_no_token() {
    let srv = InProcessServer::start(opts()).await;
    let c = srv.client(None).await;
    let info = c
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    let md = info.metadata().clone();
    let info = info.into_inner();
    assert_eq!(info.protocol_versions, vec!["scion.v1".to_owned()]);
    assert_eq!(info.repo, srv.config().server.repo);
    let ctx = srv.ctx();
    assert_eq!(info.policy_epoch, ctx.policy.epoch());
    assert_eq!(info.redaction_epoch, ctx.redactions.epoch());
    let keys: Vec<(String, Vec<u8>)> = info
        .token_public_keys
        .iter()
        .map(|k| (k.kid.clone(), k.ed25519.clone()))
        .collect();
    let want: Vec<(String, Vec<u8>)> = srv
        .keyring
        .public_keys()
        .iter()
        .map(|k| (k.kid.clone(), k.ed25519_public.to_vec()))
        .collect();
    assert_eq!(keys, want);
    let limits = info.limits.unwrap();
    assert_eq!(limits.max_blob_bytes, srv.config().limits.max_blob_bytes);
    assert_eq!(limits.max_parents, srv.config().limits.max_parents);
    assert_eq!(info.github_client_id, srv.config().auth.github_client_id);
    assert_eq!(
        info.github_oauth_base_url,
        srv.config().auth.github_oauth_base_url
    );
    let e = epochs(&md).expect("trailers on GetServerInfo");
    assert_eq!(e.tier, Tier::Server);
    assert_eq!(e.policy, ctx.policy.epoch());

    // Login without a token reaches the handler: an empty credential is INVALID_ARGUMENT,
    // not UNAUTHENTICATED.
    let err = c
        .auth()
        .login(v1::LoginRequest { credential: None })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        scion_proto::error_from_status(&err).subcode(),
        Some(scion_core::subcode::MISSING_FIELD)
    );
    assert!(epochs(err.metadata()).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn every_protected_rpc_requires_a_token() {
    let srv = InProcessServer::start(opts()).await;
    let channel = srv.client(None).await.channel();
    let rpcs = descriptor::rpcs();
    assert_eq!(rpcs.len(), 30, "scion.v1 has 30 RPCs");
    let open = [
        "/scion.v1.ObjectService/GetServerInfo",
        "/scion.v1.AuthService/Login",
    ];
    let expired = {
        let (t, _) = srv
            .keyring
            .issue(
                &alice(),
                std::time::Duration::from_secs(1),
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10),
            )
            .unwrap();
        t
    };
    for rpc in &rpcs {
        let (status, md) = raw_call(channel.clone(), rpc, None).await;
        if open.contains(&rpc.path.as_str()) {
            assert_ne!(
                status.code(),
                tonic::Code::Unauthenticated,
                "{} must not need a token",
                rpc.path
            );
        } else {
            assert_eq!(
                status.code(),
                tonic::Code::Unauthenticated,
                "{} without a token",
                rpc.path
            );
            let e = scion_proto::error_from_status(&status);
            assert_eq!(e.code, scion_core::ErrorCode::Unauthenticated);
            assert!(!e.retryable());
            for bad in ["garbage", expired.as_str()] {
                let (status, _) = raw_call(channel.clone(), rpc, Some(bad)).await;
                assert_eq!(
                    status.code(),
                    tonic::Code::Unauthenticated,
                    "{} with a bad token",
                    rpc.path
                );
            }
        }
        let e = epochs(&md).unwrap_or_else(|| panic!("{}: missing trailers", rpc.path));
        assert_eq!(e.tier, Tier::Server);
    }
    // With a valid token every RPC gets past the interceptor.
    let token = srv.token_for(&alice());
    for rpc in rpcs.iter().filter(|r| !open.contains(&r.path.as_str())) {
        let (status, md) = raw_call(channel.clone(), rpc, Some(&token)).await;
        assert_ne!(
            status.code(),
            tonic::Code::Unauthenticated,
            "{} with a valid token",
            rpc.path
        );
        assert!(epochs(&md).is_some(), "{}: missing trailers", rpc.path);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trailers_on_success_error_and_streaming() {
    let srv = InProcessServer::start(opts()).await;
    let ctx = srv.ctx();
    let want_policy = ctx.policy.epoch();

    // Success (unary): WhoAmI.
    let c = srv.client(Some(&alice())).await;
    let r = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    let e = epochs(r.metadata()).expect("success trailers");
    assert_eq!(
        (e.policy, e.redaction, e.tier),
        (want_policy, 0, Tier::Server)
    );
    assert_eq!(c.last_epochs(), Some(e));

    // Error: UNAUTHENTICATED from the interceptor.
    let anon = srv.client(None).await;
    let err = anon
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let e = epochs(err.metadata()).expect("error trailers");
    assert_eq!(e.policy, want_policy);

    // Streaming: GetObjects (stub or real) through the full stack.
    let res = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![],
            offset: None,
            length: None,
        })
        .await;
    match res {
        Ok(resp) => {
            let mut s = resp.into_inner();
            while s.message().await.unwrap().is_some() {}
            let t = s.trailers().await.unwrap().expect("stream trailers");
            assert!(epochs(&t).is_some());
        }
        Err(status) => {
            assert!(epochs(status.metadata()).is_some(), "{status:?}");
        }
    }
    // A server-streaming error response also carries them.
    let err = anon
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![],
            offset: None,
            length: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(epochs(err.metadata()).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn plaintext_server_is_reachable_over_unix_socket() {
    let srv = InProcessServer::start(ServerTestOpts {
        tls: false,
        ..opts()
    })
    .await;
    assert!(matches!(srv.endpoint, scion_proto::Endpoint::Unix(_)));
    assert!(srv.tls.is_none());
    let c = srv.client(Some(&alice())).await;
    let me = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(me.into_inner().principal, "github:alice");
}

#[tokio::test(flavor = "multi_thread")]
async fn request_id_is_echoed() {
    let srv = InProcessServer::start(opts()).await;
    let c = srv.client(None).await;
    let mut req = tonic::Request::new(v1::GetServerInfoRequest {});
    req.metadata_mut()
        .insert("x-request-id", "rid-core-rpc-1".parse().unwrap());
    let r = c.objects().get_server_info(req).await.unwrap();
    assert_eq!(r.metadata().get("x-request-id").unwrap(), "rid-core-rpc-1");
    let r = c
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    let generated = r.metadata().get("x-request-id").unwrap().to_str().unwrap();
    assert_eq!(generated.len(), 36, "uuid v4: {generated}");
}
