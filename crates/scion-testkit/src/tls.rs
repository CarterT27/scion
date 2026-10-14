//! Test certificates (SPEC 17 in tests; P08).

use scion_proto::TlsOptions;

/// An rcgen CA plus a server certificate for `localhost` and `127.0.0.1`.
#[derive(Clone, Debug)]
pub struct TestTls {
    /// CA certificate PEM.
    pub ca_pem: Vec<u8>,
    /// Server certificate PEM.
    pub cert_pem: Vec<u8>,
    /// Server private key PEM.
    pub key_pem: Vec<u8>,
}

impl TestTls {
    /// Generate fresh certificates (SPEC 17): a self-signed CA and a server certificate signed
    /// by it whose SANs are `localhost` (DNS) and `127.0.0.1` (IP).
    // SAFETY-ish: rcgen fails only for invalid parameters (these are fixed and valid) or when
    // the system RNG fails; a test fixture cannot proceed in either case.
    #[allow(clippy::expect_used)]
    pub fn generate() -> Self {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
            KeyPair, KeyUsagePurpose,
        };
        let mut ca_params =
            CertificateParams::new(Vec::<String>::new()).expect("empty SAN list is valid");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "scion test CA");
        let ca_key = KeyPair::generate().expect("generate CA key");
        let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA");
        let issuer = Issuer::new(ca_params, ca_key);

        let mut params =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .expect("static SANs are valid");
        params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("generate server key");
        let cert = params.signed_by(&key, &issuer).expect("sign server cert");

        TestTls {
            ca_pem: ca_cert.pem().into_bytes(),
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
        }
    }

    /// Client options trusting the CA (no domain override).
    pub fn client_options(&self) -> TlsOptions {
        TlsOptions {
            ca_pem: Some(self.ca_pem.clone()),
            domain: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_pem_material() {
        let t = TestTls::generate();
        for (pem, tag) in [
            (&t.ca_pem, "CERTIFICATE"),
            (&t.cert_pem, "CERTIFICATE"),
            (&t.key_pem, "PRIVATE KEY"),
        ] {
            let s = String::from_utf8(pem.clone()).unwrap();
            assert!(s.contains(&format!("-----BEGIN {tag}-----")), "{s}");
        }
        let o = t.client_options();
        assert_eq!(o.ca_pem.as_deref(), Some(t.ca_pem.as_slice()));
        assert!(o.domain.is_none());
        // Fresh material every time.
        assert_ne!(TestTls::generate().ca_pem, t.ca_pem);
    }
}
