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
    /// Generate fresh certificates.
    pub fn generate() -> Self {
        todo!("P08")
    }

    /// Client options trusting the CA.
    pub fn client_options(&self) -> TlsOptions {
        todo!("P08")
    }
}
