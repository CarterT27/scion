//! TLS from PEM (SPEC 17: TLS is REQUIRED between clients/daemons and servers; P23).
//!
//! [`rustls_config`] parses and validates the certificate chain and private key with rustls
//! (ALPN `h2`), so a bad file fails startup with a precise message. [`tonic_config`] validates
//! the same way and returns the tonic server TLS configuration used by the listener.

use scion_core::{ErrorCode, Result, ScionError};
use std::sync::Arc;

fn tls_error(m: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, m)
}

/// Parse a PEM certificate chain (SPEC 17).
fn certs(cert_pem: &[u8]) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let mut rd = cert_pem;
    let certs = rustls_pemfile::certs(&mut rd)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| tls_error(format!("invalid TLS certificate PEM: {e}")))?;
    if certs.is_empty() {
        return Err(tls_error("TLS certificate PEM holds no certificate"));
    }
    Ok(certs)
}

/// Parse a PEM private key (PKCS#8, PKCS#1 or SEC1; SPEC 17). Key material never appears in
/// error messages.
fn key(key_pem: &[u8]) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let mut rd = key_pem;
    rustls_pemfile::private_key(&mut rd)
        .map_err(|_| tls_error("invalid TLS private key PEM"))?
        .ok_or_else(|| tls_error("TLS private key PEM holds no private key"))
}

/// A rustls server configuration from PEM certificate chain and key, with ALPN `h2` for gRPC
/// (SPEC 7.1, 17). INVALID_ARGUMENT for malformed PEM or a key that does not match.
pub fn rustls_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_error(format!("TLS configuration: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs(cert_pem)?, key(key_pem)?)
        .map_err(|e| tls_error(format!("TLS certificate/key rejected: {e}")))?;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(cfg)
}

/// The tonic server TLS configuration for the gRPC listener (SPEC 17), after validating the
/// PEM material with [`rustls_config`].
pub fn tonic_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<tonic::transport::ServerTlsConfig> {
    rustls_config(cert_pem, key_pem)?;
    Ok(tonic::transport::ServerTlsConfig::new()
        .identity(tonic::transport::Identity::from_pem(cert_pem, key_pem)))
}

/// Self-signed test material: a CA and a server certificate for `localhost` and `127.0.0.1`
/// signed by it, as `(ca_pem, cert_pem, key_pem)` (SPEC 17 in tests; amendment O-1: generated
/// with rcgen directly because scion-server cannot depend on scion-testkit).
pub fn generate_test_tls() -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    let err = |e: rcgen::Error| tls_error(format!("test certificate generation: {e}"));
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "scion-server test CA");
    let ca_key = KeyPair::generate().map_err(err)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(err)?;
    let issuer = Issuer::new(ca_params, ca_key);

    let mut params = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
        .map_err(err)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().map_err(err)?;
    let cert = params.signed_by(&key, &issuer).map_err(err)?;
    Ok((
        ca_cert.pem().into_bytes(),
        cert.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_from_generated_pem() {
        let (_ca, cert, key) = generate_test_tls().unwrap();
        let cfg = rustls_config(&cert, &key).unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"h2".to_vec()]);
        tonic_config(&cert, &key).unwrap();
    }

    #[test]
    fn rejects_bad_material() {
        let (_ca, cert, key) = generate_test_tls().unwrap();
        let (_ca2, _cert2, other_key) = generate_test_tls().unwrap();
        for (c, k) in [
            (&b""[..], &key[..]),
            (
                &cert[..],
                &b"-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n"[..],
            ),
            (&cert[..], &b""[..]),
            (&cert[..], &other_key[..]),
        ] {
            let e = rustls_config(c, k).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
            assert!(!e.message.contains("PRIVATE KEY-----\n"), "{e}");
        }
    }
}
