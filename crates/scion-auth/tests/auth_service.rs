//! SPEC 19.11 service login: right secret succeeds, wrong and unknown fail, and both paths use
//! the constant-time primitive (feature `test-hooks`). Also SPEC 17 / PLAN 6.9: `Secret` never
//! prints its value.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

use scion_auth::{Secret, ServicePrincipals, ct_eq, ct_eq_calls};
use sha2::{Digest, Sha256};

fn digest_hex(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn principals() -> ServicePrincipals {
    ServicePrincipals::new(vec![
        ("ci".into(), digest_hex("ci-secret-value")),
        ("importer".into(), digest_hex("imp0rt")),
    ])
    .unwrap()
}

/// Runs `f` and returns how many `ct_eq` calls it made. Other tests in this binary may call
/// `ct_eq` concurrently, so only a lower bound is asserted.
fn calls_during(f: impl FnOnce() -> bool) -> (bool, u64) {
    let before = ct_eq_calls();
    let r = f();
    (r, ct_eq_calls() - before)
}

#[test]
fn right_secret_succeeds_and_uses_ct_eq() {
    let sp = principals();
    let (ok, n) = calls_during(|| sp.verify("ci", b"ci-secret-value"));
    assert!(ok);
    assert!(n >= 1, "right path must call ct_eq");
    assert!(sp.verify("importer", b"imp0rt"));
}

#[test]
fn wrong_secret_fails_and_uses_ct_eq() {
    let sp = principals();
    let (ok, n) = calls_during(|| sp.verify("ci", b"ci-secret-valuf"));
    assert!(!ok);
    assert!(n >= 1, "wrong path must call ct_eq");
    // Another principal's secret is wrong too.
    assert!(!sp.verify("ci", b"imp0rt"));
    assert!(!sp.verify("ci", b""));
}

#[test]
fn unknown_name_fails_and_uses_ct_eq() {
    let sp = principals();
    let (ok, n) = calls_during(|| sp.verify("nobody", b"ci-secret-value"));
    assert!(!ok);
    assert!(
        n >= 1,
        "unknown-name path must call ct_eq against the dummy digest"
    );
    assert!(!sp.verify("", b""));
    assert!(!sp.verify("CI", b"ci-secret-value"));
}

#[test]
fn ct_eq_counts_every_call() {
    let before = ct_eq_calls();
    assert!(ct_eq(b"a", b"a"));
    assert!(!ct_eq(b"a", b"b"));
    assert!(ct_eq_calls() >= before + 2);
}

#[test]
fn invalid_configuration_is_rejected() {
    let upper = digest_hex("x").to_uppercase();
    assert!(ServicePrincipals::new(vec![("ci".into(), upper)]).is_err());
    assert!(ServicePrincipals::new(vec![("ci".into(), "abc".into())]).is_err());
    assert!(
        ServicePrincipals::new(vec![
            ("ci".into(), digest_hex("a")),
            ("ci".into(), digest_hex("b"))
        ])
        .is_err()
    );
}

#[test]
fn secret_debug_never_contains_the_secret() {
    let value = "s3cr3t-Value-0123456789";
    let s = Secret::new(value.into());
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Config {
        name: String,
        secret: Secret,
    }
    let c = Config {
        name: "ci".into(),
        secret: s,
    };
    for out in [
        format!("{c:?}"),
        format!("{c:#?}"),
        format!("{}", c.secret),
        format!("{:?}", c.secret),
    ] {
        assert!(!out.contains(value), "{out}");
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(out.contains("***"), "{out}");
    }
    assert_eq!(c.secret.expose(), value);
}
