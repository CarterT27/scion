//! scion-history authorization traits over scion-policy (SPEC 12.4, 14; PLAN 6.4; P26).
//!
//! - [`PolicyWriteAuthz`] answers CreateChangeset / Merge path writes (SPEC 8.1 step 7, 12.4)
//!   and the implementation-defined `import_mode` permission (PLAN 6.4: `admin.refs` plus write
//!   on `/`) against ONE policy snapshot taken when the request starts.
//! - [`PolicyLandAuthz`] answers `update` / `force` / `write` for UpdateRef and one land attempt
//!   against one snapshot.
//! - [`PolicyLandAuthzSource`] snapshots [`PolicyHandle::current`] on EVERY call, so a land
//!   re-reads the live policy on each attempt (SPEC 8.4 "Authorization is checked in
//!   `Resolving` on every attempt against the current policy").

use scion_core::Principal;
use scion_history::{LandAuthorizer, LandAuthorizerSource, WriteAuthorizer};
use scion_object::{RefName, RepoPath};
use scion_policy::{AdminPerm, Policy, PolicyHandle};
use std::sync::Arc;

/// [`WriteAuthorizer`] over one policy snapshot (SPEC 8.1 step 7, 12.4; PLAN 6.4).
pub struct PolicyWriteAuthz {
    policy: Arc<Policy>,
    who: Principal,
}

impl PolicyWriteAuthz {
    /// Authorize `who` against `policy`.
    pub fn new(policy: Arc<Policy>, who: Principal) -> Self {
        PolicyWriteAuthz { policy, who }
    }
}

impl WriteAuthorizer for PolicyWriteAuthz {
    /// SPEC 6.2, 12.4: the longest matching `write` rule for `path` grants the principal.
    fn can_write(&self, path: &RepoPath) -> bool {
        self.policy.can_write(&self.who, path)
    }

    /// PLAN 6.4 (SPEC 14): `import_mode` requires `admin.refs` AND write on `/`.
    fn can_import(&self) -> bool {
        self.policy.can_admin(&self.who, AdminPerm::Refs)
            && self.policy.can_write(&self.who, &RepoPath::root())
    }
}

/// [`LandAuthorizer`] over one policy snapshot (SPEC 8.4, 8.5, 12.4). Additive to PLAN 2.14:
/// the snapshot handed out by [`PolicyLandAuthzSource::current`] and used by UpdateRef.
pub struct PolicyLandAuthz {
    policy: Arc<Policy>,
    who: Principal,
}

impl PolicyLandAuthz {
    /// Authorize `who` against `policy`.
    pub fn new(policy: Arc<Policy>, who: Principal) -> Self {
        PolicyLandAuthz { policy, who }
    }
}

impl LandAuthorizer for PolicyLandAuthz {
    /// SPEC 6.2, 12.4: `update` on the ref (exact name beats patterns; longest pattern wins).
    fn can_update(&self, r: &RefName) -> bool {
        self.policy.can_update_ref(&self.who, r)
    }

    /// SPEC 8.5, 12.4: `force` on the ref (non-fast-forward updates).
    fn can_force(&self, r: &RefName) -> bool {
        self.policy.can_force_ref(&self.who, r)
    }

    /// SPEC 12.4: write on a delta path of a rebased land member.
    fn can_write(&self, p: &RepoPath) -> bool {
        self.policy.can_write(&self.who, p)
    }
}

/// [`LandAuthorizerSource`] re-reading the policy handle on every land attempt (SPEC 8.4).
pub struct PolicyLandAuthzSource {
    handle: Arc<PolicyHandle>,
    who: Principal,
}

impl PolicyLandAuthzSource {
    /// Authorize `who` against the live policy.
    pub fn new(handle: Arc<PolicyHandle>, who: Principal) -> Self {
        PolicyLandAuthzSource { handle, who }
    }
}

impl LandAuthorizerSource for PolicyLandAuthzSource {
    /// A fresh [`PolicyLandAuthz`] over `PolicyHandle::current()` (SPEC 8.4: every attempt).
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        Arc::new(PolicyLandAuthz::new(self.handle.current(), self.who.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:admin", "github:half"]
admins = ["github:admin", "github:restricted"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["github:admin"]
write = ["github:admin"]

[refs."main"]
update = ["group:eng"]
force = []

[refs."release/*"]
update = ["group:admins"]
force = ["group:admins"]

[admin]
refs = ["group:admins", "github:half"]
"#;

    fn gh(login: &str) -> Principal {
        Principal::GitHub {
            login: login.into(),
            id: None,
        }
    }

    fn path(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn write_authz_follows_longest_prefix() {
        let p = Arc::new(Policy::parse(ACL).unwrap());
        let alice = PolicyWriteAuthz::new(p.clone(), gh("alice"));
        assert!(alice.can_write(&path("src/a.rs")));
        assert!(!alice.can_write(&path("secrets/key")));
        assert!(alice.can_write(&path("secrets2/key")));
        let admin = PolicyWriteAuthz::new(p.clone(), gh("admin"));
        assert!(admin.can_write(&path("secrets/key")));
        let nobody = PolicyWriteAuthz::new(p, gh("mallory"));
        assert!(!nobody.can_write(&path("src/a.rs")));
    }

    #[test]
    fn import_needs_admin_refs_and_root_write() {
        let p = Arc::new(Policy::parse(ACL).unwrap());
        // admin.refs + write on "/".
        assert!(PolicyWriteAuthz::new(p.clone(), gh("admin")).can_import());
        // admin.refs (via direct grant) + write on "/" (eng).
        assert!(PolicyWriteAuthz::new(p.clone(), gh("half")).can_import());
        // Write on "/" but no admin.refs.
        assert!(!PolicyWriteAuthz::new(p.clone(), gh("alice")).can_import());
        // admin.refs but no write on "/".
        assert!(!PolicyWriteAuthz::new(p, gh("restricted")).can_import());
    }

    #[test]
    fn land_authz_per_ref_pattern() {
        let p = Arc::new(Policy::parse(ACL).unwrap());
        let main = RefName::main();
        let rel = RefName::parse("release/1.0").unwrap();
        let other = RefName::parse("feature/x").unwrap();
        let alice = PolicyLandAuthz::new(p.clone(), gh("alice"));
        assert!(alice.can_update(&main));
        assert!(!alice.can_force(&main));
        assert!(!alice.can_update(&rel));
        assert!(!alice.can_update(&other));
        assert!(alice.can_write(&path("a")));
        assert!(!alice.can_write(&path("secrets/a")));
        let admin = PolicyLandAuthz::new(p, gh("admin"));
        assert!(admin.can_update(&rel));
        assert!(admin.can_force(&rel));
        assert!(!admin.can_force(&main));
    }

    #[test]
    fn land_source_rereads_policy_every_call() {
        let handle = PolicyHandle::from_policy(Policy::parse(ACL).unwrap());
        let src = PolicyLandAuthzSource::new(handle.clone(), gh("alice"));
        let main = RefName::main();
        assert!(src.current().can_update(&main));
        // A snapshot taken before the swap keeps its answer; a new call sees the new policy.
        let before = src.current();
        let dir = tempfile::tempdir().unwrap();
        let acl_path = dir.path().join("acl.toml");
        std::fs::write(&acl_path, ACL).unwrap();
        let live = PolicyHandle::load(&acl_path).unwrap();
        let src = PolicyLandAuthzSource::new(live.clone(), gh("alice"));
        let snap = src.current();
        assert!(snap.can_update(&main));
        std::fs::write(&acl_path, ACL.replace("update = [\"group:eng\"]", "update = []"))
            .unwrap();
        assert!(live.reload_now().unwrap());
        assert!(snap.can_update(&main), "old snapshot is immutable");
        assert!(!src.current().can_update(&main), "current() re-reads");
        assert!(before.can_update(&main));
    }
}
