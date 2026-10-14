//! Signed tokens (SPEC 12.1; PLAN 6.1; P06).
//!
//! Encoding (PLAN 6.1): PASETO `v4.public` (Ed25519). The payload is JSON with `sub` (principal id
//! string), optional `gid` (GitHub numeric id), and `iat`/`exp` as RFC 3339 timestamps (whole
//! seconds). The footer is JSON `{"kid":"<16 hex>"}`, where kid is the first 16 hex characters of
//! BLAKE3 over the raw 32-byte public key. The footer is covered by the signature.

use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use pasetors::Public;
use pasetors::keys::{AsymmetricPublicKey, AsymmetricSecretKey};
use pasetors::token::UntrustedToken;
use pasetors::version4::{PublicToken, V4};
use scion_core::{ErrorCode, Principal, Result, ScionError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Token claims: `sub`, optional GitHub numeric `gid`, `iat`, `exp`, and the signing `kid`
/// from the footer.
#[derive(Clone, Debug, PartialEq)]
pub struct Claims {
    /// Subject principal.
    pub sub: Principal,
    /// GitHub numeric user id, for GitHub principals.
    pub gid: Option<u64>,
    /// Issued at.
    pub iat: SystemTime,
    /// Expires at.
    pub exp: SystemTime,
    /// Key id: first 16 hex of BLAKE3(public key).
    pub kid: String,
}

/// A verification key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKeyInfo {
    /// Key id.
    pub kid: String,
    /// Raw Ed25519 public key.
    pub ed25519_public: [u8; 32],
}

impl PublicKeyInfo {
    /// The info for a raw Ed25519 public key, with its kid derived as in PLAN 6.1 (first 16 hex
    /// characters of BLAKE3 over the 32 key bytes; SPEC 12.1).
    pub fn from_public(ed25519_public: [u8; 32]) -> Self {
        PublicKeyInfo {
            kid: kid_of(&ed25519_public),
            ed25519_public,
        }
    }
}

/// First 16 hex characters of BLAKE3(public key) (PLAN 6.1).
fn kid_of(ed25519_public: &[u8; 32]) -> String {
    let h = blake3::hash(ed25519_public);
    let mut s = hex::encode(h.as_bytes());
    s.truncate(16);
    s
}

/// Signing keys: the first key signs; all keys verify (rotation, SPEC 12.1).
#[derive(Clone)]
pub struct Keyring {
    keys: Vec<(String, ed25519_dalek::SigningKey)>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gid: Option<u64>,
    iat: String,
    exp: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Footer {
    kid: String,
}

fn unauthenticated(why: &str) -> ScionError {
    ScionError::unauthenticated(format!("invalid token: {why}"))
}

fn config_error(m: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, m)
}

fn truncate_to_seconds(t: SystemTime) -> SystemTime {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => UNIX_EPOCH + Duration::from_secs(d.as_secs()),
        Err(_) => UNIX_EPOCH,
    }
}

fn format_time(t: SystemTime) -> Result<String> {
    if t < UNIX_EPOCH {
        return Err(config_error("token timestamps must not precede 1970"));
    }
    Ok(humantime::format_rfc3339_seconds(t).to_string())
}

fn parse_time(s: &str) -> Result<SystemTime> {
    humantime::parse_rfc3339(s).map_err(|_| unauthenticated("bad timestamp"))
}

fn with_kids(keys: Vec<ed25519_dalek::SigningKey>) -> Vec<(String, ed25519_dalek::SigningKey)> {
    keys.into_iter()
        .map(|k| (kid_of(&k.verifying_key().to_bytes()), k))
        .collect()
}

impl Keyring {
    /// Load PKCS#8 PEM Ed25519 keys (SPEC 12.1, PLAN 6.1). The first path is the signing key; at
    /// least one path is required. Unreadable or malformed files are INVALID_ARGUMENT
    /// (configuration errors); messages never contain key material.
    pub fn from_files(paths: &[PathBuf]) -> Result<Self> {
        if paths.is_empty() {
            return Err(config_error("keyring needs at least one signing key"));
        }
        let mut keys = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let pem = std::fs::read_to_string(p)
                .map_err(|e| config_error(format!("cannot read signing key #{i}: {}", e.kind())))?;
            let k = ed25519_dalek::SigningKey::from_pkcs8_pem(&pem).map_err(|_| {
                config_error(format!(
                    "signing key #{i} is not a PKCS#8 PEM Ed25519 private key"
                ))
            })?;
            keys.push(k);
        }
        Ok(Keyring {
            keys: with_kids(keys),
        })
    }

    /// `n` fresh random keys (tests; `n == 0` is treated as 1). SPEC 12.1.
    pub fn generate_for_tests(n: usize) -> Self {
        let keys = (0..n.max(1))
            .map(|_| ed25519_dalek::SigningKey::from_bytes(&rand::random::<[u8; 32]>()))
            .collect();
        Keyring {
            keys: with_kids(keys),
        }
    }

    /// A keyring holding the given keys of this keyring, in the given order (rotation harnesses,
    /// SPEC 12.1: e.g. `[1, 0]` promotes key 1 to signer, `[0]` drops key 1). An empty or
    /// out-of-range selection is INVALID_ARGUMENT.
    pub fn select(&self, indexes: &[usize]) -> Result<Self> {
        if indexes.is_empty() {
            return Err(config_error("keyring needs at least one signing key"));
        }
        let mut keys = Vec::with_capacity(indexes.len());
        for &i in indexes {
            let k = self
                .keys
                .get(i)
                .ok_or_else(|| config_error(format!("no key #{i}")))?;
            keys.push(k.clone());
        }
        Ok(Keyring { keys })
    }

    /// Write every key as a PKCS#8 PEM file under `dir` (mode 0600 on Unix); returns the paths
    /// in key order (SPEC 12.1 harness support).
    pub fn write_pem_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::with_capacity(self.keys.len());
        for (i, (kid, k)) in self.keys.iter().enumerate() {
            let pem = k
                .to_pkcs8_pem(LineEnding::LF)
                .map_err(|_| config_error("cannot encode signing key"))?;
            let path = dir.join(format!("signing-{i}-{kid}.pem"));
            write_private(&path, pem.as_bytes()).map_err(|e| {
                ScionError::new(
                    ErrorCode::Unavailable,
                    format!("cannot write signing key #{i}: {}", e.kind()),
                )
            })?;
            out.push(path);
        }
        Ok(out)
    }

    /// Issue a token for `sub` valid for `ttl` from `now` (SPEC 12.1). Timestamps are truncated
    /// to whole seconds so the returned claims equal what [`Verifier::verify`] yields; `gid` is
    /// the GitHub id carried by `sub`, if any.
    pub fn issue(
        &self,
        sub: &Principal,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<(String, Claims)> {
        let iat = truncate_to_seconds(now);
        let exp = iat
            .checked_add(Duration::from_secs(ttl.as_secs()))
            .ok_or_else(|| config_error("token lifetime overflows"))?;
        let gid = match sub {
            Principal::GitHub { id, .. } => *id,
            Principal::Service(_) => None,
        };
        let mut claims = Claims {
            sub: sub.clone(),
            gid,
            iat,
            exp,
            kid: String::new(),
        };
        let token = self.sign(&claims)?;
        claims.kid = self
            .keys
            .first()
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        Ok((token, claims))
    }

    /// Sign explicit claims with the first key (SPEC 12.1). The footer carries the first key's
    /// kid; the `kid` field of `c` is ignored. Sub-second precision of `iat`/`exp` is dropped.
    pub fn sign(&self, c: &Claims) -> Result<String> {
        let (kid, key) = self
            .keys
            .first()
            .ok_or_else(|| config_error("keyring has no signing key"))?;
        let payload = Payload {
            sub: c.sub.id_string(),
            gid: c.gid,
            iat: format_time(c.iat)?,
            exp: format_time(c.exp)?,
        };
        let message = serde_json::to_vec(&payload)
            .map_err(|_| config_error("cannot encode token payload"))?;
        let footer = serde_json::to_vec(&Footer { kid: kid.clone() })
            .map_err(|_| config_error("cannot encode token footer"))?;
        let sk = AsymmetricSecretKey::<V4>::from(&key.to_keypair_bytes())
            .map_err(|_| config_error("invalid signing key"))?;
        PublicToken::sign(&sk, &message, Some(&footer), None)
            .map_err(|_| config_error("token signing failed"))
    }

    /// Every verification key (published in ServerInfo), in key order (SPEC 12.1).
    pub fn public_keys(&self) -> Vec<PublicKeyInfo> {
        self.keys
            .iter()
            .map(|(kid, k)| PublicKeyInfo {
                kid: kid.clone(),
                ed25519_public: k.verifying_key().to_bytes(),
            })
            .collect()
    }

    /// A verifier over every key (SPEC 12.1).
    pub fn verifier(&self) -> Verifier {
        Verifier::new(self.public_keys())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Verifies tokens against a set of public keys (server and daemon, SPEC 12.1).
#[derive(Clone)]
pub struct Verifier {
    keys: Vec<PublicKeyInfo>,
}

impl Verifier {
    /// A verifier over `keys`.
    pub fn new(keys: Vec<PublicKeyInfo>) -> Self {
        Verifier { keys }
    }

    /// Verify a token at `now`: UNAUTHENTICATED for malformed, unknown-kid, bad-signature,
    /// tampered or expired (`exp <= now`) tokens (SPEC 12.1, 19.11).
    pub fn verify(&self, token: &str, now: SystemTime) -> Result<Claims> {
        let untrusted = UntrustedToken::<Public, V4>::try_from(token)
            .map_err(|_| unauthenticated("malformed"))?;
        // The footer is untrusted until the signature (which covers it) verifies; it only
        // selects the candidate key.
        let footer: Footer = serde_json::from_slice(untrusted.untrusted_footer())
            .map_err(|_| unauthenticated("malformed footer"))?;
        let key = self
            .keys
            .iter()
            .find(|k| k.kid == footer.kid)
            .ok_or_else(|| unauthenticated("unknown key"))?;
        let pk = AsymmetricPublicKey::<V4>::from(&key.ed25519_public)
            .map_err(|_| unauthenticated("unusable key"))?;
        let trusted = PublicToken::verify(&pk, &untrusted, None, None)
            .map_err(|_| unauthenticated("bad signature"))?;
        let payload: Payload = serde_json::from_str(trusted.payload())
            .map_err(|_| unauthenticated("malformed payload"))?;
        let iat = parse_time(&payload.iat)?;
        let exp = parse_time(&payload.exp)?;
        if exp <= now {
            return Err(ScionError::unauthenticated("token expired"));
        }
        let mut sub = Principal::parse(&payload.sub).map_err(|_| unauthenticated("bad subject"))?;
        if let Principal::GitHub { id, .. } = &mut sub {
            *id = payload.gid;
        }
        Ok(Claims {
            sub,
            gid: payload.gid,
            iat,
            exp,
            kid: key.kid.clone(),
        })
    }

    /// The verification keys (SPEC 12.1).
    pub fn keys(&self) -> &[PublicKeyInfo] {
        &self.keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn kid_is_16_hex_of_blake3() {
        let k = Keyring::generate_for_tests(1);
        let pk = k.public_keys()[0].clone();
        let full = hex::encode(blake3::hash(&pk.ed25519_public).as_bytes());
        assert_eq!(pk.kid, full[..16]);
        assert_eq!(PublicKeyInfo::from_public(pk.ed25519_public), pk);
    }

    #[test]
    fn token_layout() {
        let k = Keyring::generate_for_tests(1);
        let sub = Principal::Service("ci".into());
        let (tok, claims) = k
            .issue(&sub, Duration::from_secs(60), t(1_700_000_000))
            .unwrap();
        assert!(tok.starts_with("v4.public."));
        let parts: Vec<&str> = tok.split('.').collect();
        assert_eq!(parts.len(), 4);
        let untrusted = UntrustedToken::<Public, V4>::try_from(tok.as_str()).unwrap();
        let footer: serde_json::Value =
            serde_json::from_slice(untrusted.untrusted_footer()).unwrap();
        assert_eq!(footer, serde_json::json!({ "kid": claims.kid }));
        let payload: serde_json::Value =
            serde_json::from_slice(untrusted.untrusted_payload()).unwrap();
        assert_eq!(payload["sub"], "service:ci");
        assert_eq!(payload["iat"], "2023-11-14T22:13:20Z");
        assert_eq!(payload["exp"], "2023-11-14T22:14:20Z");
        assert!(payload.get("gid").is_none());
    }

    #[test]
    fn subsecond_truncation_roundtrips() {
        let k = Keyring::generate_for_tests(1);
        let sub = Principal::GitHub {
            login: "alice".into(),
            id: Some(7),
        };
        let now = t(1_700_000_000) + Duration::from_millis(999);
        let (tok, claims) = k.issue(&sub, Duration::from_secs(10), now).unwrap();
        assert_eq!(k.verifier().verify(&tok, now).unwrap(), claims);
        assert_eq!(claims.iat, t(1_700_000_000));
        assert_eq!(claims.gid, Some(7));
    }

    #[test]
    fn select_errors() {
        let k = Keyring::generate_for_tests(2);
        assert!(k.select(&[]).is_err());
        assert!(k.select(&[2]).is_err());
        assert_eq!(k.select(&[1]).unwrap().public_keys()[0], k.public_keys()[1]);
    }
}
