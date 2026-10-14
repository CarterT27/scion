//! RPC log events and path hashing (SPEC 15.1; PLAN 6.9; P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// The REQUIRED SPEC 15.1 fields of one RPC log event.
#[derive(Clone, Debug, Default)]
pub struct RpcLogEvent {
    /// Request id.
    pub request_id: String,
    /// RPC name.
    pub rpc: String,
    /// `server` or `daemon`.
    pub tier: &'static str,
    /// Principal id string, when authenticated.
    pub principal: Option<String>,
    /// Repository name.
    pub repo: String,
    /// Status code string.
    pub status: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Changeset id text, when applicable.
    pub changeset_id: Option<String>,
    /// Ref name, logged as `ref`.
    pub ref_name: Option<String>,
    /// Ref version, when applicable.
    pub ref_version: Option<u64>,
    /// Land attempt number, when applicable.
    pub land_attempt: Option<u32>,
}

impl RpcLogEvent {
    /// Emit at INFO with target `scion::rpc`.
    pub fn emit(&self) {
        todo!("P04")
    }
}

/// 16 hex characters of BLAKE3 of `path`: the ONLY way paths may appear in logs (PLAN 6.9).
pub fn path_hash(path: &[u8]) -> String {
    todo!("P04")
}
