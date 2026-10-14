//! [`ScionClient`]: a `scion.v1` client over a Unix socket or HTTPS (SPEC 7.1; P08).

use crate::endpoint::{Endpoint, TlsOptions};
use crate::trailers::Epochs;
use crate::v1;
use scion_core::Result;
use std::sync::Arc;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

/// Adds `authorization: Bearer <token>` to every request when a token is set (SPEC 7.1).
#[derive(Clone, Debug, Default)]
pub struct BearerInterceptor {
    #[allow(dead_code)]
    token: Option<Arc<str>>,
}

/// A tower service wrapper around the channel that captures the epochs from response trailers
/// (and trailers-only headers) and counts calls.
#[derive(Clone, Debug)]
pub struct EpochCapture<S> {
    #[allow(dead_code)]
    inner: S,
    #[allow(dead_code)]
    state: Arc<ClientState>,
}

/// Shared per-client state: last observed epochs and the call counter.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct ClientState {
    pub(crate) last_epochs: std::sync::Mutex<Option<Epochs>>,
    pub(crate) calls: std::sync::atomic::AtomicU64,
}

/// The transport stack used by every generated client of [`ScionClient`].
pub type Transport = InterceptedService<EpochCapture<Channel>, BearerInterceptor>;

/// A connected `scion.v1` client (SPEC 7.1). Cheap to clone.
#[derive(Clone)]
pub struct ScionClient {
    channel: Channel,
    #[allow(dead_code)]
    token: Option<Arc<str>>,
    #[allow(dead_code)]
    state: Arc<ClientState>,
}

impl ScionClient {
    /// Connect to `ep` (Unix socket or HTTPS with rustls), optionally authenticating with
    /// `token`.
    pub async fn connect(
        ep: &Endpoint,
        tls: Option<TlsOptions>,
        token: Option<String>,
    ) -> Result<Self> {
        let _ = (ep, tls, token);
        todo!("P08")
    }

    /// The same connection with a different bearer token.
    pub fn with_token(&self, token: Option<String>) -> Self {
        let _ = token;
        todo!("P08")
    }

    /// ObjectService client (SPEC 7.3).
    pub fn objects(&self) -> v1::object_service_client::ObjectServiceClient<Transport> {
        todo!("P08")
    }

    /// QueryService client (SPEC 7.4).
    pub fn query(&self) -> v1::query_service_client::QueryServiceClient<Transport> {
        todo!("P08")
    }

    /// WriteService client (SPEC 7.5).
    pub fn write(&self) -> v1::write_service_client::WriteServiceClient<Transport> {
        todo!("P08")
    }

    /// RefService client (SPEC 7.6).
    pub fn refs(&self) -> v1::ref_service_client::RefServiceClient<Transport> {
        todo!("P08")
    }

    /// AuthService client (SPEC 7.7).
    pub fn auth(&self) -> v1::auth_service_client::AuthServiceClient<Transport> {
        todo!("P08")
    }

    /// AdminService client (SPEC 7.7).
    pub fn admin(&self) -> v1::admin_service_client::AdminServiceClient<Transport> {
        todo!("P08")
    }

    /// Epochs captured from the most recent response trailers (SPEC 7.1).
    pub fn last_epochs(&self) -> Option<Epochs> {
        todo!("P08")
    }

    /// Number of RPCs issued through this client (and its clones).
    pub fn calls(&self) -> u64 {
        todo!("P08")
    }

    /// Reset the call counter.
    pub fn reset_calls(&self) {
        todo!("P08")
    }

    /// The underlying channel.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }
}
