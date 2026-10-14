//! Service principals (SPEC 12.3; P06).

use crate::secret::ct_eq;
use scion_core::{ErrorCode, Result, ScionError};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Digest compared against for unknown names, so the unknown-name path does the same work as
/// the known-name path. It is not the SHA-256 of any known input.
const DUMMY_DIGEST: [u8; 32] = [0xA5; 32];

/// Configured service principals: `(name, sha256 lowercase hex of the secret)` (SPEC 12.3).
pub struct ServicePrincipals {
    entries: BTreeMap<String, [u8; 32]>,
}

impl ServicePrincipals {
    /// Validate entries (64 lowercase hex digests, non-empty unique names); INVALID_ARGUMENT
    /// otherwise (SPEC 12.3).
    pub fn new(entries: Vec<(String, String)>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for (name, digest_hex) in entries {
            if name.is_empty() {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    "service principal name must not be empty",
                ));
            }
            let well_formed = digest_hex.len() == 64
                && digest_hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            let mut digest = [0u8; 32];
            if !well_formed || hex::decode_to_slice(&digest_hex, &mut digest).is_err() {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "service principal {name:?}: secret digest must be 64 lowercase hex characters"
                    ),
                ));
            }
            if map.insert(name.clone(), digest).is_some() {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!("duplicate service principal {name:?}"),
                ));
            }
        }
        Ok(ServicePrincipals { entries: map })
    }

    /// Always hashes `secret` and compares with [`crate::ct_eq`]; unknown names compare against
    /// a dummy digest so timing does not reveal which names exist (SPEC 12.3, 19.11).
    pub fn verify(&self, name: &str, secret: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(secret).into();
        let (known, expected) = match self.entries.get(name) {
            Some(d) => (true, d),
            None => (false, &DUMMY_DIGEST),
        };
        let matches = ct_eq(&digest, expected);
        matches & known
    }

    /// Number of configured principals.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no principals are configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> String {
        hex::encode(Sha256::digest(s.as_bytes()))
    }

    #[test]
    fn validation() {
        assert!(ServicePrincipals::new(vec![]).unwrap().is_empty());
        let ok = ServicePrincipals::new(vec![("ci".into(), h("s"))]).unwrap();
        assert_eq!(ok.len(), 1);
        for bad in [
            vec![("ci".into(), h("s").to_uppercase())],
            vec![("ci".into(), h("s")[..62].to_owned())],
            vec![("ci".into(), format!("{}zz", &h("s")[..62]))],
            vec![(String::new(), h("s"))],
            vec![("ci".into(), h("a")), ("ci".into(), h("b"))],
        ] {
            assert_eq!(
                ServicePrincipals::new(bad).err().unwrap().code,
                ErrorCode::InvalidArgument
            );
        }
    }

    #[test]
    fn verify_paths() {
        let sp = ServicePrincipals::new(vec![("ci".into(), h("s3cret"))]).unwrap();
        assert!(sp.verify("ci", b"s3cret"));
        assert!(!sp.verify("ci", b"s3cret!"));
        assert!(!sp.verify("cd", b"s3cret"));
        assert!(!sp.verify("", b""));
    }
}
