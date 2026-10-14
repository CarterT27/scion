//! SPEC 19.11 token items and key rotation (PLAN 2.10, 6.1).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use scion_auth::{Claims, Keyring, PublicKeyInfo, Verifier};
use scion_core::{ErrorCode, Principal};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HOUR: Duration = Duration::from_secs(3600);

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_760_000_000)
}

fn alice() -> Principal {
    Principal::GitHub {
        login: "alice".into(),
        id: Some(4242),
    }
}

fn assert_unauth(v: &Verifier, tok: &str, at: SystemTime) {
    let e = v.verify(tok, at).expect_err("token must be rejected");
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");
}

/// Rebuilds a token from (possibly modified) payload and footer bytes, keeping the signature.
fn reassemble(
    tok: &str,
    edit_payload: impl Fn(&mut Vec<u8>),
    edit_footer: impl Fn(&mut Vec<u8>),
) -> String {
    let parts: Vec<&str> = tok.split('.').collect();
    assert_eq!(parts.len(), 4);
    let body = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    let (msg, sig) = body.split_at(body.len() - 64);
    let mut msg = msg.to_vec();
    edit_payload(&mut msg);
    let mut footer = URL_SAFE_NO_PAD.decode(parts[3]).unwrap();
    edit_footer(&mut footer);
    let mut m = msg;
    m.extend_from_slice(sig);
    format!(
        "v4.public.{}.{}",
        URL_SAFE_NO_PAD.encode(&m),
        URL_SAFE_NO_PAD.encode(&footer)
    )
}

#[test]
fn valid_token_verifies_with_sub_and_gid() {
    let k = Keyring::generate_for_tests(1);
    let (tok, claims) = k.issue(&alice(), 12 * HOUR, now()).unwrap();
    assert_eq!(claims.sub, alice());
    assert_eq!(claims.gid, Some(4242));
    assert_eq!(claims.iat, now());
    assert_eq!(claims.exp, now() + 12 * HOUR);
    assert_eq!(claims.kid, k.public_keys()[0].kid);
    let got = k.verifier().verify(&tok, now()).unwrap();
    assert_eq!(got, claims);
    assert_eq!(got.sub.to_string(), "github:alice");

    let svc = Principal::Service("ci".into());
    let (tok, claims) = k.issue(&svc, HOUR, now()).unwrap();
    let got = k
        .verifier()
        .verify(&tok, now() + HOUR - Duration::from_secs(1))
        .unwrap();
    assert_eq!(got, claims);
    assert_eq!(got.gid, None);
}

#[test]
fn sign_explicit_claims() {
    let k = Keyring::generate_for_tests(1);
    let c = Claims {
        sub: Principal::GitHub {
            login: "bob".into(),
            id: Some(9),
        },
        gid: Some(9),
        iat: now(),
        exp: now() + HOUR,
        kid: "ignored".into(),
    };
    let tok = k.sign(&c).unwrap();
    let got = k.verifier().verify(&tok, now()).unwrap();
    assert_eq!(got.sub, c.sub);
    assert_eq!(got.gid, c.gid);
    assert_eq!((got.iat, got.exp), (c.iat, c.exp));
    assert_eq!(got.kid, k.public_keys()[0].kid);
}

#[test]
fn expired_tokens_are_rejected() {
    let k = Keyring::generate_for_tests(1);
    let (tok, claims) = k.issue(&alice(), HOUR, now()).unwrap();
    let v = k.verifier();
    assert!(v.verify(&tok, claims.exp - Duration::from_secs(1)).is_ok());
    assert_unauth(&v, &tok, claims.exp); // exp <= now
    assert_unauth(&v, &tok, claims.exp + Duration::from_secs(1));
    let (zero, _) = k.issue(&alice(), Duration::ZERO, now()).unwrap();
    assert_unauth(&v, &zero, now());
}

#[test]
fn malformed_tokens_are_rejected() {
    let k = Keyring::generate_for_tests(1);
    let v = k.verifier();
    let (tok, _) = k.issue(&alice(), HOUR, now()).unwrap();
    let no_footer = tok.rsplit_once('.').unwrap().0.to_owned();
    let local = tok.replacen("v4.public.", "v4.local.", 1);
    let v2 = tok.replacen("v4.", "v2.", 1);
    let truncated = tok[..tok.len() / 2].to_owned();
    for bad in [
        "",
        "garbage",
        "v4.public.",
        "v4.public..",
        "v4.public.!!!!.e30",
        "v4.public.AAAA.e30",
        no_footer.as_str(),
        local.as_str(),
        v2.as_str(),
        truncated.as_str(),
        "Bearer x",
    ] {
        assert_unauth(&v, bad, now());
    }
    // Footer that is not JSON / lacks kid.
    let bad_footer = reassemble(&tok, |_| {}, |f| *f = b"not json".to_vec());
    assert_unauth(&v, &bad_footer, now());
    let no_kid = reassemble(&tok, |_| {}, |f| *f = b"{}".to_vec());
    assert_unauth(&v, &no_kid, now());
}

#[test]
fn wrong_key_tokens_are_rejected() {
    let a = Keyring::generate_for_tests(1);
    let b = Keyring::generate_for_tests(1);
    let (tok, _) = b.issue(&alice(), HOUR, now()).unwrap();
    // Unknown kid.
    assert_unauth(&a.verifier(), &tok, now());
    // Known kid, but the listed public key is a different key: bad signature.
    let forged = Verifier::new(vec![PublicKeyInfo {
        kid: b.public_keys()[0].kid.clone(),
        ed25519_public: a.public_keys()[0].ed25519_public,
    }]);
    assert_unauth(&forged, &tok, now());
    // Empty verifier.
    assert_unauth(&Verifier::new(vec![]), &tok, now());
}

#[test]
fn tampered_payload_is_rejected() {
    let k = Keyring::generate_for_tests(1);
    let v = k.verifier();
    let (tok, _) = k.issue(&alice(), HOUR, now()).unwrap();
    // Escalate the subject.
    let t1 = reassemble(
        &tok,
        |m| {
            let s = String::from_utf8(m.clone())
                .unwrap()
                .replace("github:alice", "github:admin");
            *m = s.into_bytes();
        },
        |_| {},
    );
    assert_unauth(&v, &t1, now());
    // Extend the expiry.
    let t2 = reassemble(
        &tok,
        |m| {
            let s = String::from_utf8(m.clone())
                .unwrap()
                .replace("\"exp\":\"2025", "\"exp\":\"2099");
            *m = s.into_bytes();
        },
        |_| {},
    );
    assert_ne!(t2, tok);
    assert_unauth(&v, &t2, now());
    // Flip one bit in every byte position of payload+signature.
    let parts: Vec<&str> = tok.split('.').collect();
    let body = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    for i in 0..body.len() {
        let mut b = body.clone();
        b[i] ^= 0x01;
        let t = format!("v4.public.{}.{}", URL_SAFE_NO_PAD.encode(&b), parts[3]);
        assert_unauth(&v, &t, now());
    }
}

#[test]
fn tampered_footer_is_rejected() {
    let k = Keyring::generate_for_tests(2);
    let v = k.verifier();
    let (tok, claims) = k.issue(&alice(), HOUR, now()).unwrap();
    let other_kid = k.public_keys()[1].kid.clone();
    // Point the footer at the other listed key.
    let swapped = reassemble(
        &tok,
        |_| {},
        |f| *f = format!("{{\"kid\":\"{other_kid}\"}}").into_bytes(),
    );
    assert_unauth(&v, &swapped, now());
    // Same kid, different JSON bytes (whitespace): signature covers the footer bytes.
    let spaced = reassemble(
        &tok,
        |_| {},
        |f| *f = format!("{{ \"kid\": \"{}\" }}", claims.kid).into_bytes(),
    );
    assert_unauth(&v, &spaced, now());
    // Extra footer field.
    let extra = reassemble(
        &tok,
        |_| {},
        |f| *f = format!("{{\"kid\":\"{}\",\"x\":1}}", claims.kid).into_bytes(),
    );
    assert_unauth(&v, &extra, now());
}

#[test]
fn rotation() {
    let k = Keyring::generate_for_tests(2);
    let keys = k.public_keys();
    // A deployment where key 2 was the signer (old key).
    let old = k.select(&[1]).unwrap();
    let (old_tok, old_claims) = old.issue(&alice(), HOUR, now()).unwrap();
    assert_eq!(old_claims.kid, keys[1].kid);

    // Rotated: key 1 signs, key 2 still listed -> old tokens verify.
    let rotated = k.select(&[0, 1]).unwrap();
    assert_eq!(rotated.public_keys(), keys);
    let got = rotated.verifier().verify(&old_tok, now()).unwrap();
    assert_eq!(got, old_claims);
    // New tokens are signed by the first key.
    let (new_tok, new_claims) = rotated.issue(&alice(), HOUR, now()).unwrap();
    assert_eq!(new_claims.kid, keys[0].kid);
    assert_eq!(
        rotated.verifier().verify(&new_tok, now()).unwrap().kid,
        keys[0].kid
    );

    // Key 2 removed -> old tokens fail, new ones still verify.
    let pruned = k.select(&[0]).unwrap();
    assert_unauth(&pruned.verifier(), &old_tok, now());
    assert!(pruned.verifier().verify(&new_tok, now()).is_ok());

    // The same through a Verifier built from published keys (daemon side).
    let daemon = Verifier::new(vec![keys[0].clone()]);
    assert_unauth(&daemon, &old_tok, now());
    assert_eq!(daemon.keys(), &keys[..1]);
}

#[test]
fn pem_files_roundtrip_and_rotation_via_files() {
    let dir = tempfile::tempdir().unwrap();
    let k = Keyring::generate_for_tests(3);
    let paths = k.write_pem_files(dir.path()).unwrap();
    assert_eq!(paths.len(), 3);
    for p in &paths {
        let pem = std::fs::read_to_string(p).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"), "{pem:.40}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let loaded = Keyring::from_files(&paths).unwrap();
    assert_eq!(loaded.public_keys(), k.public_keys());
    let (tok, _) = loaded.issue(&alice(), HOUR, now()).unwrap();
    assert!(k.verifier().verify(&tok, now()).is_ok());

    // Reordered files: the first file signs.
    let reordered = Keyring::from_files(&[paths[2].clone(), paths[0].clone()]).unwrap();
    let (_, c) = reordered.issue(&alice(), HOUR, now()).unwrap();
    assert_eq!(c.kid, k.public_keys()[2].kid);

    // Errors: no keys, missing file, not a key.
    assert_eq!(
        Keyring::from_files(&[]).err().unwrap().code,
        ErrorCode::InvalidArgument
    );
    let missing = dir.path().join("missing.pem");
    assert_eq!(
        Keyring::from_files(&[missing]).err().unwrap().code,
        ErrorCode::InvalidArgument
    );
    let junk = dir.path().join("junk.pem");
    std::fs::write(
        &junk,
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let e = Keyring::from_files(&[junk]).err().unwrap();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

#[test]
fn external_pkcs8_pem_is_accepted() {
    // RFC 8410 section 10.3 example private key.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("rfc8410.pem");
    std::fs::write(
        &p,
        "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n",
    )
    .unwrap();
    let k = Keyring::from_files(&[p]).unwrap();
    let pk = &k.public_keys()[0];
    assert_eq!(
        hex::encode(pk.ed25519_public),
        "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1"
    );
    assert_eq!(pk.kid.len(), 16);
    assert_eq!(PublicKeyInfo::from_public(pk.ed25519_public).kid, pk.kid);
}
