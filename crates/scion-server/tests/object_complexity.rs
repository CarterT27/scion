//! SPEC 19.18 complexity gate (P25): object authorization reads `object_paths` rows only for
//! the requested object, so `object_paths_rows_read` equals that object's row count at every
//! repository size (10^3 and 10^5 rows of other objects).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;

use bytes::Bytes;
use object_support::{ACL, alice, dave, sec};
use scion_core::{ChangeId, Kind, ObjectId, Principal};
use scion_object::RepoPath;
use scion_policy::{Policy, PolicyHandle};
use scion_server::RedactionSet;
use scion_server::authz::{AccessClass, ObjectAuthorizer, ReadDecision};
use scion_store::{ChangesetBundle, ChangesetRow, MemOptions, ObjectPathRow, Storage};
use std::time::SystemTime;

fn blob(i: u64) -> ObjectId {
    ObjectId::hash_blob(&i.to_be_bytes())
}

fn row(object: ObjectId, path: &str, cs: ObjectId) -> ObjectPathRow {
    ObjectPathRow {
        object,
        path: RepoPath::parse(path.as_bytes()).unwrap(),
        first_changeset: cs,
        at: SystemTime::UNIX_EPOCH,
    }
}

/// A repository whose `object_paths` table holds `n` rows of other objects, plus the target
/// blob at three paths (two restricted, one public) and a restricted-only blob at one path.
async fn repo(n: u64) -> (Storage, ObjectId, ObjectId) {
    let (storage, _) = Storage::in_memory(MemOptions::default());
    let cs = ObjectId::hash_encoded(Kind::Changeset, b"fake changeset");
    let target = ObjectId::hash_blob(b"target");
    let restricted = ObjectId::hash_blob(b"restricted");
    let mut rows: Vec<ObjectPathRow> = (0..n)
        .map(|i| row(blob(i), &format!("d{}/f{i}", i % 97), cs))
        .collect();
    rows.push(row(target, "secrets/a", cs));
    rows.push(row(target, "secrets/b", cs));
    rows.push(row(target, "src/main.rs", cs));
    rows.push(row(restricted, "secrets/only", cs));
    storage
        .meta
        .commit_changeset(ChangesetBundle {
            row: ChangesetRow {
                id: cs,
                encoded: Bytes::from_static(b"fake changeset"),
                generation: 2,
                skip: Vec::new(),
                change_id: ChangeId([7; 16]),
                created_by: "github:alice".into(),
                created_at: SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths: rows,
            derived: Vec::new(),
        })
        .await
        .unwrap();
    (storage, target, restricted)
}

async fn rows_read(
    authz: &ObjectAuthorizer,
    storage: &Storage,
    p: &Principal,
    id: &ObjectId,
) -> (ReadDecision, u64) {
    authz.clear_cache();
    let before = storage.counters.snapshot();
    let d = authz.can_read(p, id).await.unwrap();
    let delta = storage.counters.snapshot() - before;
    (d, delta.object_paths_rows_read)
}

async fn gate(n: u64) {
    let (storage, target, restricted) = repo(n).await;
    let policy = PolicyHandle::from_policy(Policy::parse(ACL).unwrap());
    let authz = ObjectAuthorizer::new(
        storage.clone(),
        policy,
        RedactionSet::new(storage.meta.clone()),
    );
    // Allowed, denied and restricted decisions each read exactly the object's own rows.
    assert_eq!(
        rows_read(&authz, &storage, &alice(), &target).await,
        (ReadDecision::Allow(AccessClass::Root), 3)
    );
    assert_eq!(
        rows_read(&authz, &storage, &dave(), &target).await,
        (ReadDecision::Deny, 3)
    );
    assert_eq!(
        rows_read(&authz, &storage, &sec(), &target).await,
        (ReadDecision::Allow(AccessClass::Restricted), 3)
    );
    assert_eq!(
        rows_read(&authz, &storage, &alice(), &restricted).await,
        (ReadDecision::Deny, 1)
    );
    assert_eq!(
        rows_read(&authz, &storage, &alice(), &ObjectId::hash_blob(b"absent")).await,
        (ReadDecision::Deny, 0)
    );
    // A cached decision reads no rows at all.
    let _ = authz.can_read(&alice(), &target).await.unwrap();
    let before = storage.counters.snapshot();
    assert_eq!(
        authz.can_read(&alice(), &target).await.unwrap(),
        ReadDecision::Allow(AccessClass::Root)
    );
    assert_eq!(
        (storage.counters.snapshot() - before).object_paths_rows_read,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authorization_rows_read_independent_of_repo_size_1e3() {
    gate(1_000).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn authorization_rows_read_independent_of_repo_size_1e5() {
    gate(100_000).await;
}
