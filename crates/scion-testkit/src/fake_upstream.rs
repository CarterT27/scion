//! In-memory `scion.v1` upstream for daemon and client tests (PLAN 2.21; P08).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use bytes::Bytes;
use scion_core::ObjectId;
use scion_proto::{Endpoint, TlsOptions};
use std::sync::Arc;

/// Options of a [`FakeUpstream`].
#[derive(Clone)]
pub struct FakeUpstreamOpts {
    /// Serve TLS (via [`crate::TestTls`]).
    pub tls: bool,
    /// Maps a bearer token to a principal id string (`None` = UNAUTHENTICATED).
    pub principal_of: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// Token public keys published by GetServerInfo.
    pub public_keys: Vec<(String, [u8; 32])>,
}

impl Default for FakeUpstreamOpts {
    fn default() -> Self {
        FakeUpstreamOpts {
            tls: false,
            principal_of: Arc::new(|_| None),
            public_keys: Vec::new(),
        }
    }
}

/// One recorded request.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRequest {
    /// Full gRPC method path.
    pub method: String,
    /// Object/changeset ids in the request.
    pub ids: Vec<ObjectId>,
    /// Caller principal, when authenticated.
    pub principal: Option<String>,
}

/// ObjectService (+ GetChangesets/Resolve, CheckAccess/WhoAmI, ListRedactions) on
/// `127.0.0.1:0`; every other method is UNIMPLEMENTED.
pub struct FakeUpstream {
    _private: (),
}

impl FakeUpstream {
    /// Start serving.
    pub async fn start(opts: FakeUpstreamOpts) -> Self {
        todo!("P08")
    }

    /// The endpoint to connect to.
    pub fn endpoint(&self) -> Endpoint {
        todo!("P08")
    }

    /// TLS client options when serving TLS.
    pub fn tls(&self) -> Option<TlsOptions> {
        todo!("P08")
    }

    /// Insert an object (`restricted` sets the access class).
    pub fn insert_object(&self, id: ObjectId, raw: Bytes, restricted: bool) {
        todo!("P08")
    }

    /// Insert a changeset encoding.
    pub fn insert_changeset(&self, id: ObjectId, encoded: Bytes) {
        todo!("P08")
    }

    /// Set a ref.
    pub fn set_ref(&self, name: &str, target: ObjectId, version: u64) {
        todo!("P08")
    }

    /// Deny `id` to `principal`.
    pub fn deny(&self, principal: &str, id: ObjectId) {
        todo!("P08")
    }

    /// Mark `id` redacted (and list it in ListRedactions).
    pub fn redact(&self, id: ObjectId) {
        todo!("P08")
    }

    /// Serve corrupted bytes for `id`.
    pub fn corrupt(&self, id: ObjectId) {
        todo!("P08")
    }

    /// Kill GetObjects streams after `n` bytes (`None` = never).
    pub fn kill_stream_after_bytes(&self, n: Option<u64>) {
        todo!("P08")
    }

    /// When down, every RPC fails UNAVAILABLE.
    pub fn set_down(&self, down: bool) {
        todo!("P08")
    }

    /// Epochs stamped on responses (tier server).
    pub fn set_epochs(&self, policy: u64, redaction: u64) {
        todo!("P08")
    }

    /// Every recorded request.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        todo!("P08")
    }

    /// Forget recorded requests.
    pub fn reset_requests(&self) {
        todo!("P08")
    }
}
