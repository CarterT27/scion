//! Upstream connection (SPEC 3.3: a tier talks only to the tier above it; P16).

use async_trait::async_trait;
use scion_core::Result;
use scion_proto::{Endpoint, ScionClient, TlsOptions};

/// Connects to the upstream tier (SPEC 3.3).
#[async_trait]
pub trait UpstreamConnector: Send + Sync {
    /// A connected client (without a token; callers attach the caller's token).
    async fn connect(&self) -> Result<ScionClient>;
    /// Every destination ever dialed (network-policy test, SPEC 19.8).
    fn destinations(&self) -> Vec<String>;
}

/// Dials one configured endpoint and records destinations (SPEC 3.3, 19.8).
pub struct DirectUpstream {
    endpoint: Endpoint,
    tls: Option<TlsOptions>,
    dialed: parking_lot::Mutex<Vec<String>>,
}

impl DirectUpstream {
    /// An upstream at `endpoint` (SPEC 3.3).
    pub fn new(endpoint: Endpoint, tls: Option<TlsOptions>) -> Self {
        DirectUpstream {
            endpoint,
            tls,
            dialed: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl UpstreamConnector for DirectUpstream {
    /// Dial the configured endpoint (recorded before the attempt, so failed dials count too).
    async fn connect(&self) -> Result<ScionClient> {
        let dest = self.endpoint.to_string();
        {
            let mut d = self.dialed.lock();
            if !d.contains(&dest) {
                d.push(dest);
            }
        }
        ScionClient::connect(&self.endpoint, self.tls.clone(), None).await
    }

    /// Distinct destinations dialed so far, in first-dial order.
    fn destinations(&self) -> Vec<String> {
        self.dialed.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_destinations_even_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ep = Endpoint::Unix(dir.path().join("none.sock"));
        let up = DirectUpstream::new(ep.clone(), None);
        assert!(up.destinations().is_empty());
        assert!(up.connect().await.is_err());
        assert!(up.connect().await.is_err());
        assert_eq!(up.destinations(), vec![ep.to_string()]);
    }
}
