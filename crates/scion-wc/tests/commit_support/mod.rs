//! Shared helpers for the P28 `commit_*`, `rebase_*`, `land_*` and `recover_*` tests: a real
//! in-process server, scenarios over it, and a server-state validity check.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test helpers

use scion_core::{Kind, Mode, ObjectId, Principal, empty_tree_id};
use scion_object::{Canonical, Changeset, RefName, RepoPath};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_wc::testing::Scenario;
use std::collections::BTreeSet;

/// The default writer.
pub fn alice() -> Principal {
    Principal::parse("github:alice").unwrap()
}

/// An admin (reads/writes everything, may redact).
pub fn admin() -> Principal {
    Principal::parse("github:admin").unwrap()
}

/// Ref `main`.
pub fn main_ref() -> RefName {
    RefName::main()
}

/// Parse a repository path.
pub fn rp(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

/// A real server with the default ACL (eng reads/writes `/`).
pub async fn server() -> InProcessServer {
    InProcessServer::start(ServerTestOpts::default()).await
}

/// A real server with a custom ACL.
pub async fn server_with_acl(acl: &str) -> InProcessServer {
    InProcessServer::start(ServerTestOpts {
        acl_toml: acl.to_owned(),
        ..ServerTestOpts::default()
    })
    .await
}

/// A scenario for `who` on `srv`.
pub async fn scenario(srv: &InProcessServer, who: &Principal) -> Scenario {
    Scenario::new(srv.client(Some(who)).await).unwrap()
}

/// SPEC 4.5 / 16.2 spot check of the server state: every changeset's parents exist, and every
/// object reachable from its root tree exists in the objects table.
pub async fn validate_server(srv: &InProcessServer) {
    let mem = srv.mem.as_ref().expect("in-memory storage");
    let objects: BTreeSet<ObjectId> = mem.meta.all_objects().iter().map(|o| o.id).collect();
    let changesets = mem.meta.all_changesets();
    let ids: BTreeSet<ObjectId> = changesets.iter().map(|c| c.id).collect();
    let history = srv.ctx().history.clone();
    let mut seen_trees: BTreeSet<ObjectId> = BTreeSet::new();
    for row in &changesets {
        let cs = Changeset::decode(&row.encoded).unwrap();
        assert_eq!(cs.id(), row.id, "changeset row id");
        for p in &cs.parents {
            assert!(ids.contains(p), "missing parent {p} of {}", row.id);
        }
        assert!(objects.contains(&cs.meta), "missing meta of {}", row.id);
        let mut stack = vec![cs.root_tree];
        while let Some(t) = stack.pop() {
            if !seen_trees.insert(t) {
                continue;
            }
            assert!(
                t == empty_tree_id() || objects.contains(&t),
                "missing tree {t} of {}",
                row.id
            );
            let tree = history.tree(&t).await.unwrap();
            for e in &tree.entries {
                if e.mode == Mode::Tree {
                    stack.push(e.id);
                } else {
                    assert_eq!(e.id.kind(), Kind::Blob);
                    assert!(
                        objects.contains(&e.id),
                        "missing blob {} of {}",
                        e.id,
                        row.id
                    );
                }
            }
        }
    }
}
