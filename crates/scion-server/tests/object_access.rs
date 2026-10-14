//! SPEC 19.12 object access control (P25): retroactive policy changes with no reindex, the
//! HasObjects existence probe, unreferenced uploads, and tree authorization by directory path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;

use object_support::*;
use scion_object::RepoPath;
use scion_proto::v1;

async fn subtree(
    srv: &scion_server::testing::InProcessServer,
    root: &scion_core::ObjectId,
    p: &str,
) -> scion_core::ObjectId {
    srv.ctx()
        .history
        .lookup(root, &RepoPath::parse(p.as_bytes()).unwrap())
        .await
        .unwrap()
        .unwrap()
        .id
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_a_restriction_is_retroactive_without_reindex() {
    let srv = start().await;
    let ctx = srv.ctx();
    let cs = commit(
        &srv,
        &[
            ("restricted/only.txt", b"only under the new prefix"),
            ("restricted/shared.txt", b"shared content"),
            ("pub/shared.txt", b"shared content"),
            ("pub/plain.txt", b"plain"),
        ],
    )
    .await;
    let only = cs.blobs["restricted/only.txt"];
    let shared = cs.blobs["pub/shared.txt"];
    assert_eq!(shared, cs.blobs["restricted/shared.txt"]);
    let plain = cs.blobs["pub/plain.txt"];
    let restricted_tree = subtree(&srv, &cs.root_tree, "restricted").await;
    let a = srv.client(Some(&alice())).await;
    // Everything readable (and the decisions cached) before the change.
    for id in [only, shared, plain, restricted_tree, cs.root_tree] {
        assert_eq!(status_of(&a, &id).await, v1::ItemStatus::Ok);
    }
    assert!(ctx.authz.cached_decisions() >= 5);

    let rows_before = srv.mem.as_ref().unwrap().meta.table_counts();
    let writes_before = ctx.storage.counters.snapshot().objects_written;
    let epoch_before = ctx.policy.epoch();
    srv.set_policy(ACL_RESTRICTED).unwrap();
    assert_ne!(ctx.policy.epoch(), epoch_before);

    // Immediately: objects only under the new prefix are unreadable, objects also present at
    // an unrestricted path stay readable.
    let (items, err) = get_objects(
        &a,
        &[only, shared, plain, restricted_tree, cs.root_tree],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(err.is_none());
    let st: Vec<v1::ItemStatus> = items.iter().map(|i| i.status).collect();
    use v1::ItemStatus::{Denied, Ok};
    assert_eq!(st, vec![Denied, Ok, Ok, Denied, Ok]);
    assert_eq!(items[1].access_class, v1::AccessClass::Root);
    assert_eq!(has(&a, &[only, shared]).await, vec![false, true]);

    // The security group reads the new prefix with the restricted class.
    let s = srv.client(Some(&sec())).await;
    let (items, _) = get_objects(&s, &[only, shared, plain], None, None)
        .await
        .unwrap();
    let st: Vec<v1::ItemStatus> = items.iter().map(|i| i.status).collect();
    assert_eq!(st, vec![Ok, Ok, Denied]);
    assert_eq!(items[0].access_class, v1::AccessClass::Restricted);
    assert_eq!(items[1].access_class, v1::AccessClass::Restricted);
    // Carol reads both "/" and the new prefix: the shared blob is root class for her.
    let (items, _) = get_objects(
        &srv.client(Some(&carol())).await,
        &[only, shared],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(items[0].access_class, v1::AccessClass::Restricted);
    assert_eq!(items[1].access_class, v1::AccessClass::Root);

    // No reindex: no object_paths rows and no objects were written.
    let rows_after = srv.mem.as_ref().unwrap().meta.table_counts();
    assert_eq!(rows_after.object_paths, rows_before.object_paths);
    assert_eq!(
        ctx.storage.counters.snapshot().objects_written,
        writes_before
    );

    // Reverting the policy restores access just as immediately.
    srv.set_policy(ACL).unwrap();
    assert_eq!(status_of(&a, &only).await, Ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn has_objects_existence_probe_and_duplicate_upload() {
    let srv = start().await;
    let secret = b"top secret content".to_vec();
    let cs = commit(&srv, &[("secrets/s.txt", &secret)]).await;
    let sid = cs.blobs["secrets/s.txt"];
    let a = srv.client(Some(&alice())).await;
    // Exists, unreadable, not uploaded by alice: false.
    assert_eq!(has(&a, &[sid]).await, vec![false]);
    assert_eq!(status_of(&a, &sid).await, v1::ItemStatus::Denied);

    // A duplicate upload succeeds and its result is indistinguishable from a fresh upload.
    let (_, dup) = upload_blob(&a, &secret).await;
    let fresh_content = b"brand new content".to_vec();
    let (fid, fresh) = upload_blob(&a, &fresh_content).await;
    assert_eq!(dup.status, fresh.status);
    assert_eq!(dup.error_code, fresh.error_code);
    assert_eq!(dup.message, fresh.message);
    assert_eq!(dup.status, v1::ItemStatus::Ok as i32);

    // Now alice uploaded both: true; uploading grants no read access.
    assert_eq!(has(&a, &[sid, fid]).await, vec![true, true]);
    assert_eq!(status_of(&a, &sid).await, v1::ItemStatus::Denied);
    // Other principals are unaffected.
    assert_eq!(
        has(&srv.client(Some(&bob())).await, &[sid, fid]).await,
        vec![false, false]
    );
    assert_eq!(
        has(&srv.client(Some(&carol())).await, &[sid]).await,
        vec![true]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreferenced_uploads_are_readable_by_no_one() {
    let srv = start().await;
    let b = srv.client(Some(&bob())).await;
    let content = b"uploaded, never committed".to_vec();
    let (id, r) = upload_blob(&b, &content).await;
    assert_eq!(r.status, v1::ItemStatus::Ok as i32);
    for p in [bob(), alice(), carol(), admin(), dave()] {
        let c = srv.client(Some(&p)).await;
        assert_eq!(status_of(&c, &id).await, v1::ItemStatus::NotFound, "{p}");
    }
    assert_eq!(has(&b, &[id]).await, vec![true]);
    assert_eq!(
        has(&srv.client(Some(&alice())).await, &[id]).await,
        vec![false]
    );
    let ca = b
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: srv.token_for(&bob()),
            object_ids: ids(&[id]),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ca.allowed, vec![false]);

    // Once a changeset references it at a readable path it is readable (denials are not
    // cached, so this takes effect at once).
    let cs = commit(&srv, &[("pub/now.txt", &content)]).await;
    assert_eq!(cs.blobs["pub/now.txt"], id);
    assert_eq!(read_ok(&b, &id).await, content);
}

#[tokio::test(flavor = "multi_thread")]
async fn trees_authorize_by_directory_path() {
    let srv = start().await;
    let cs = commit(
        &srv,
        &[
            ("secrets/deep/k.txt", b"k"),
            ("src/lib.rs", b"fn main() {}"),
        ],
    )
    .await;
    let secrets = subtree(&srv, &cs.root_tree, "secrets").await;
    let deep = subtree(&srv, &cs.root_tree, "secrets/deep").await;
    let src = subtree(&srv, &cs.root_tree, "src").await;
    let root = cs.root_tree;

    let st = |items: Vec<Item>| items.iter().map(|i| i.status).collect::<Vec<_>>();
    use v1::ItemStatus::{Denied, Ok};
    let a = srv.client(Some(&alice())).await;
    let (items, _) = get_objects(&a, &[root, src, secrets, deep], None, None)
        .await
        .unwrap();
    assert_eq!(st(items), vec![Ok, Ok, Denied, Denied]);
    let c = srv.client(Some(&carol())).await;
    let (items, _) = get_objects(&c, &[root, src, secrets, deep], None, None)
        .await
        .unwrap();
    assert_eq!(items[0].access_class, v1::AccessClass::Root);
    assert_eq!(items[2].access_class, v1::AccessClass::Restricted);
    assert_eq!(st(items), vec![Ok, Ok, Ok, Ok]);
    // The root tree sits at the empty path, governed by "/".
    let s = srv.client(Some(&sec())).await;
    let (items, _) = get_objects(&s, &[root, src, secrets, deep], None, None)
        .await
        .unwrap();
    assert_eq!(st(items), vec![Denied, Denied, Ok, Ok]);
    // The bootstrap root changeset's empty root tree is readable by repository readers.
    let empty = scion_core::empty_tree_id();
    assert_eq!(status_of(&a, &empty).await, Ok);
}
