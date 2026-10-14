//! RPC log events and path hashing (SPEC 15.1; PLAN 6.9; P04).

/// The tracing target of RPC log events (SPEC 15.1).
pub const RPC_TARGET: &str = "scion::rpc";

/// The `principal` value logged for unauthenticated calls (`GetServerInfo`, `Login`).
pub const ANONYMOUS_PRINCIPAL: &str = "anonymous";

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
    /// Emit at INFO with target `scion::rpc` (SPEC 15.1). `request_id`, `rpc`, `tier`,
    /// `principal` (`anonymous` when unauthenticated), `repo`, `status` and `duration_ms` are
    /// always present; `changeset_id`, `ref`, `ref_version` and `land_attempt` are present
    /// when set.
    pub fn emit(&self) {
        let principal = self.principal.as_deref().unwrap_or(ANONYMOUS_PRINCIPAL);
        tracing::info!(
            target: RPC_TARGET,
            request_id = self.request_id.as_str(),
            rpc = self.rpc.as_str(),
            tier = self.tier,
            principal = principal,
            repo = self.repo.as_str(),
            status = self.status.as_str(),
            duration_ms = self.duration_ms,
            changeset_id = self.changeset_id.as_deref(),
            r#ref = self.ref_name.as_deref(),
            ref_version = self.ref_version,
            land_attempt = self.land_attempt,
            "rpc"
        );
    }
}

/// 16 hex characters of BLAKE3 of `path`: the ONLY way paths may appear in logs (PLAN 6.9,
/// SPEC 15.1).
pub fn path_hash(path: &[u8]) -> String {
    let digest = blake3::hash(path);
    hex::encode(&digest.as_bytes()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_hash_shape() {
        let h = path_hash(b"src/secret/plan.txt");
        assert_eq!(h.len(), 16);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(h, path_hash(b"src/secret/plan.txt"));
        assert_ne!(h, path_hash(b"src/secret/plan.txT"));
        assert_eq!(&blake3::hash(b"").to_hex()[..16], path_hash(b""));
    }
}
