//! Upstream connection (SPEC 3.3: a tier talks only to the tier above it; P16).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use async_trait::async_trait;
use scion_core::Result;
use scion_proto::{Endpoint, ScionClient, TlsOptions};

/// Connects to the upstream tier.
#[async_trait]
pub trait UpstreamConnector: Send + Sync {
    /// A connected client (without a token; callers attach the caller's token).
    async fn connect(&self) -> Result<ScionClient>;
    /// Every destination ever dialed (network-policy test, SPEC 19.8).
    fn destinations(&self) -> Vec<String>;
}

/// Dials one configured endpoint and records destinations.
pub struct DirectUpstream {
    endpoint: Endpoint,
    tls: Option<TlsOptions>,
    dialed: parking_lot::Mutex<Vec<String>>,
}

impl DirectUpstream {
    /// An upstream at `endpoint`.
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
    async fn connect(&self) -> Result<ScionClient> {
        todo!("P16")
    }

    fn destinations(&self) -> Vec<String> {
        todo!("P16")
    }
}
