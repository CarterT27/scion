//! SPEC 19.17 Observability, unit level: every SPEC 15.2 metric is exported after one
//! observation; `/healthz` and `/readyz` reflect state; RPC log events carry every REQUIRED
//! SPEC 15.1 field as a JSON key; `path_hash` is stable and never the input; the log capture
//! finds planted secrets.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use scion_obs::capture::LogCapture;
use scion_obs::{Metrics, Readiness, RpcLogEvent, health_router, path_hash, serve_admin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tower::ServiceExt;

/// A readiness probe toggled by the test.
#[derive(Default)]
struct Toggle(AtomicBool);

impl Readiness for Toggle {
    fn check(&self) -> Result<(), String> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("metadata store unreachable".to_string())
        }
    }
}

struct AlwaysReady;
impl Readiness for AlwaysReady {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

/// One observation of every metric.
fn observe_all(m: &Metrics) {
    m.rpc_duration
        .with_label_values(&["GetObjects", "server", "OK"])
        .observe(0.004);
    m.cache_requests
        .with_label_values(&["daemon", "blob", "hit"])
        .inc();
    m.cache_bytes.with_label_values(&["daemon"]).set(1024);
    m.land_attempts.with_label_values(&["landed"]).inc();
    m.ref_cas_failures.with_label_values(&["main"]).inc();
    m.land_duration.observe(0.02);
    m.derive_duration
        .with_label_values(&["history", "eager"])
        .observe(0.001);
    m.derive_backlog.with_label_values(&["blame"]).set(3);
    m.redaction_epoch.with_label_values(&["server"]).set(5);
    m.redaction_lag.with_label_values(&["daemon"]).set(0.5);
    m.policy_epoch_info.with_label_values(&["42"]).set(1);
    m.object_bytes_served
        .with_label_values(&["server", "zstd"])
        .inc_by(100);
    m.upstream_requests.with_label_values(&["daemon"]).inc();
    m.integrity_failures.with_label_values(&["server"]).inc();
}

async fn get(router: &axum::Router, path: &str) -> (StatusCode, String, Option<String>) {
    let resp = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap(), ctype)
}

// ---- Metrics ---------------------------------------------------------------------------------

#[test]
fn every_15_2_metric_name_is_rendered_after_one_observation() {
    let m = Metrics::new();
    observe_all(&m);
    let text = m.render();
    for name in Metrics::NAMES {
        assert!(
            text.contains(&format!("# TYPE {name} ")),
            "{name} missing from:\n{text}"
        );
    }
    assert_eq!(Metrics::NAMES.len(), 14);
}

#[test]
fn metric_labels_match_spec_15_2() {
    let m = Metrics::new();
    observe_all(&m);
    let text = m.render();
    for needle in [
        r#"scion_rpc_duration_seconds_count{code="OK",rpc="GetObjects",tier="server"} 1"#,
        r#"scion_cache_requests_total{kind="blob",result="hit",tier="daemon"} 1"#,
        r#"scion_cache_bytes{tier="daemon"} 1024"#,
        r#"scion_land_attempts{result="landed"} 1"#,
        r#"scion_ref_cas_failures_total{ref="main"} 1"#,
        "scion_land_duration_seconds_count 1",
        r#"scion_derive_duration_seconds_count{mode="eager",type="history"} 1"#,
        r#"scion_derive_backlog{type="blame"} 3"#,
        r#"scion_redaction_epoch{tier="server"} 5"#,
        r#"scion_redaction_lag_seconds{tier="daemon"} 0.5"#,
        r#"scion_policy_epoch_info{epoch="42"} 1"#,
        r#"scion_object_bytes_served_total{encoding="zstd",tier="server"} 100"#,
        r#"scion_upstream_requests_total{tier="daemon"} 1"#,
        r#"scion_integrity_failures_total{tier="server"} 1"#,
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
    assert!(text.contains("# TYPE scion_rpc_duration_seconds histogram"));
    assert!(text.contains("# TYPE scion_cache_requests_total counter"));
    assert!(text.contains("# TYPE scion_cache_bytes gauge"));
}

#[test]
fn separate_metrics_instances_are_independent() {
    let a = Metrics::new();
    let b = Metrics::new();
    a.upstream_requests.with_label_values(&["daemon"]).inc();
    assert_eq!(b.upstream_requests.with_label_values(&["daemon"]).get(), 0);
}

// ---- Health ----------------------------------------------------------------------------------

#[tokio::test]
async fn healthz_is_200() {
    let router = health_router(Metrics::new(), Arc::new(Toggle::default()));
    let (status, _, _) = get(&router, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn readyz_follows_readiness() {
    let ready = Arc::new(Toggle::default());
    let router = health_router(Metrics::new(), ready.clone());
    let (status, body, _) = get(&router, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("metadata store unreachable"));
    // /healthz stays 200 while not ready.
    assert_eq!(get(&router, "/healthz").await.0, StatusCode::OK);
    ready.0.store(true, Ordering::SeqCst);
    assert_eq!(get(&router, "/readyz").await.0, StatusCode::OK);
    ready.0.store(false, Ordering::SeqCst);
    assert_eq!(
        get(&router, "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn metrics_endpoint_serves_text_exposition() {
    let m = Metrics::new();
    observe_all(&m);
    let router = health_router(m, Arc::new(AlwaysReady));
    let (status, body, ctype) = get(&router, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.unwrap().starts_with("text/plain"));
    for name in Metrics::NAMES {
        assert!(body.contains(name), "{name}");
    }
    assert_eq!(get(&router, "/nope").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serve_admin_binds_and_serves() {
    let ready = Arc::new(Toggle::default());
    let m = Metrics::new();
    m.land_duration.observe(0.1);
    let router = health_router(m, ready.clone());
    let (addr, handle) = serve_admin("127.0.0.1:0".parse().unwrap(), router)
        .await
        .unwrap();
    assert_ne!(addr.port(), 0);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = |p: &str| format!("http://{addr}{p}");
    assert_eq!(
        client.get(url("/healthz")).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        client.get(url("/readyz")).send().await.unwrap().status(),
        503
    );
    ready.0.store(true, Ordering::SeqCst);
    assert_eq!(
        client.get(url("/readyz")).send().await.unwrap().status(),
        200
    );
    let body = client
        .get(url("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("scion_land_duration_seconds_count 1"));
    handle.abort();
}

// ---- path_hash -------------------------------------------------------------------------------

#[test]
fn path_hash_is_stable_and_never_the_input() {
    let inputs: [&[u8]; 6] = [
        b"",
        b"a",
        b"secret/plans/q3.txt",
        b"0123456789abcdef",
        "caf\u{e9}/\u{1f600}".as_bytes(),
        &[0xff, 0x00, 0x2f],
    ];
    for input in inputs {
        let h = path_hash(input);
        assert_eq!(h, path_hash(input), "stable");
        assert_eq!(h.len(), 16);
        assert!(h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(h.as_bytes(), input, "never the input");
        if input.len() >= 4 {
            // The raw path never shows through in the hash text.
            assert!(!contains(h.as_bytes(), input));
        }
    }
    assert_ne!(path_hash(b"a"), path_hash(b"b"));
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

// ---- Logs (one test, because the capture buffer is process-global) ---------------------------

fn find_event(cap: &LogCapture, key: &str, value: &str) -> serde_json::Value {
    cap.events()
        .into_iter()
        .find(|e| e.get(key).and_then(|v| v.as_str()) == Some(value))
        .unwrap_or_else(|| panic!("no event with {key}={value} in:\n{}", cap.raw()))
}

#[test]
fn log_capture_rpc_events_and_needles() {
    let cap = LogCapture::install();
    let again = LogCapture::install(); // idempotent

    // RpcLogEvent with every field set: all REQUIRED SPEC 15.1 fields are JSON keys.
    let full = RpcLogEvent {
        request_id: "req-full-6c1f".into(),
        rpc: "Land".into(),
        tier: "server",
        principal: Some("github:octocat".into()),
        repo: "monorepo".into(),
        status: "OK".into(),
        duration_ms: 17,
        changeset_id: Some("c1abcdef".into()),
        ref_name: Some("main".into()),
        ref_version: Some(9),
        land_attempt: Some(2),
    };
    full.emit();
    let e = find_event(&cap, "request_id", "req-full-6c1f");
    for key in [
        "request_id",
        "rpc",
        "tier",
        "principal",
        "repo",
        "status",
        "duration_ms",
        "changeset_id",
        "ref",
        "ref_version",
        "land_attempt",
    ] {
        assert!(e.get(key).is_some(), "missing {key} in {e}");
    }
    assert_eq!(e["rpc"], "Land");
    assert_eq!(e["tier"], "server");
    assert_eq!(e["principal"], "github:octocat");
    assert_eq!(e["repo"], "monorepo");
    assert_eq!(e["status"], "OK");
    assert_eq!(e["duration_ms"], 17);
    assert_eq!(e["changeset_id"], "c1abcdef");
    assert_eq!(e["ref"], "main");
    assert_eq!(e["ref_version"], 9);
    assert_eq!(e["land_attempt"], 2);
    assert_eq!(e["level"], "INFO");
    assert_eq!(e["target"], "scion::rpc");

    // Minimal event: base fields present, inapplicable ones absent, anonymous principal.
    RpcLogEvent {
        request_id: "req-min-90aa".into(),
        rpc: "GetServerInfo".into(),
        tier: "daemon",
        repo: "monorepo".into(),
        status: "OK".into(),
        ..Default::default()
    }
    .emit();
    let e = find_event(&again, "request_id", "req-min-90aa");
    for key in [
        "request_id",
        "rpc",
        "tier",
        "principal",
        "repo",
        "status",
        "duration_ms",
    ] {
        assert!(e.get(key).is_some(), "missing {key} in {e}");
    }
    assert_eq!(e["principal"], "anonymous");
    assert_eq!(e["duration_ms"], 0);
    for key in ["changeset_id", "ref", "ref_version", "land_attempt"] {
        assert!(e.get(key).is_none(), "{key} should be absent in {e}");
    }

    // A planted secret is found, raw and when Debug-formatted; absent needles are not.
    let secret = "v4.public.SECRET-TOKEN-5f1e2d";
    tracing::warn!(target: "scion::test", token = %secret, "planted");
    let quoted = "pass\"word-77";
    tracing::info!(target: "other_crate", value = ?quoted, "planted debug");
    let hits = cap.find_needles(&[secret.as_bytes()]);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].contains("planted"));
    assert_eq!(cap.find_needles(&[quoted.as_bytes()]).len(), 1);
    assert!(
        cap.find_needles(&[b"never-logged-3b9c".as_slice(), b"".as_slice()])
            .is_empty()
    );
    let both = cap.find_needles(&[secret.as_bytes(), quoted.as_bytes()]);
    assert_eq!(both.len(), 2);

    // Levels: DEBUG for scion targets is captured, DEBUG elsewhere and TRACE are not.
    tracing::debug!(target: "scion::test", marker = "dbg-scion-41d2");
    tracing::debug!(target: "hyper", marker = "dbg-other-41d2");
    tracing::trace!(target: "scion::test", marker = "trace-scion-41d2");
    assert_eq!(cap.find_needles(&[b"dbg-scion-41d2"]).len(), 1);
    assert!(cap.find_needles(&[b"dbg-other-41d2"]).is_empty());
    assert!(cap.find_needles(&[b"trace-scion-41d2"]).is_empty());

    // The integrity alert (PLAN 6.10) is an ERROR event with event="integrity_alert".
    let m = Metrics::new();
    let id = scion_core::ObjectId::hash_blob(b"corrupt-me-0b7");
    m.integrity_alert("daemon", &id);
    let e = find_event(&cap, "object", &id.to_text());
    assert_eq!(e["event"], "integrity_alert");
    assert_eq!(e["level"], "ERROR");
    assert_eq!(e["tier"], "daemon");
    assert_eq!(m.integrity_failures.with_label_values(&["daemon"]).get(), 1);

    // Every captured line is one JSON object.
    let raw = cap.raw();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        assert!(
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap()
                .is_object()
        );
    }

    // clear drops everything captured so far.
    cap.clear();
    assert!(cap.find_needles(&[secret.as_bytes()]).is_empty());
    assert!(again.find_needles(&[b"req-full-6c1f"]).is_empty());
}
