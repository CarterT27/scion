//! Signed tokens (SPEC 12.1; PLAN 6.1; P06).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::{Principal, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// Signing keys: the first key signs; all keys verify (rotation, SPEC 12.1).
#[derive(Clone)]
pub struct Keyring {
    keys: Vec<(String, ed25519_dalek::SigningKey)>,
}

impl Keyring {
    /// Load PKCS#8 PEM Ed25519 keys.
    pub fn from_files(paths: &[PathBuf]) -> Result<Self> {
        todo!("P06")
    }

    /// `n` fresh random keys (tests).
    pub fn generate_for_tests(n: usize) -> Self {
        todo!("P06")
    }

    /// Write every key as a PKCS#8 PEM file under `dir`; returns the paths in key order.
    pub fn write_pem_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        todo!("P06")
    }

    /// Issue a token for `sub` valid for `ttl` from `now`.
    pub fn issue(
        &self,
        sub: &Principal,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<(String, Claims)> {
        todo!("P06")
    }

    /// Sign explicit claims with the first key.
    pub fn sign(&self, c: &Claims) -> Result<String> {
        todo!("P06")
    }

    /// Every verification key (published in ServerInfo).
    pub fn public_keys(&self) -> Vec<PublicKeyInfo> {
        todo!("P06")
    }

    /// A verifier over every key.
    pub fn verifier(&self) -> Verifier {
        todo!("P06")
    }
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
    /// tampered or expired (`exp <= now`) tokens.
    pub fn verify(&self, token: &str, now: SystemTime) -> Result<Claims> {
        todo!("P06")
    }
}
