//! SPEC 19.12 history filtering, 19.13 ReadPath on a redacted blob, and 19.1 revision
//! resolution with ambiguous prefixes (P27).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod query_support;

use query_support::*;
use scion_core::{ChangeId, ErrorCode, ErrorPayload, ObjectId, subcode};
use scion_history::{AllowAll, MetaSource, UpdateRefRequest};
use scion_object::{Canonical, RefName};
use scion_proto::v1;
use std::collections::{BTreeSet, HashMap};

async fn get_changesets(
    srv: &scion_server::testing::InProcessServer,
    who: &scion_core::Principal,
    ids: &[ObjectId],
) -> Vec<v1::ChangesetItem> {
    srv.client(Some(who))
        .await
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: ids.iter().map(idb).collect(),
        })
        .await
        .unwrap()
        .into_inner()
        .items
}

async fn diff_paths(
    srv: &scion_server::testing::InProcessServer,
    who: &scion_core::Principal,
    a: &ObjectId,
    b: &ObjectId,
) -> Vec<String> {
    let mut s = srv
        .client(Some(who))
        .await
        .query()
        .diff(v1::DiffRequest {
            a: idb(a),
            b: idb(b),
            paths: vec![],
            include_content_hunks: true,
        })
        .await
        .unwrap()
        .into_inner();
    let mut out = Vec::new();
    while let Some(item) = s.message().await.unwrap() {
        out.push(String::from_utf8(item.path).unwrap());
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn get_changesets_filters_unreadable_delta_entries() {
    let r = repo().await;
    let (cs1, row1) = r.ctx.objects.get_changeset(&r.c1).await.unwrap();
    let (_, row3) = r.ctx.objects.get_changeset(&r.c3).await.unwrap();

    // alice cannot read /secret: c1 comes back filtered, c3 (README only) in full.
    let items = get_changesets(&r.srv, &alice(), &[r.c1, r.c3]).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, idb(&r.c1));
    assert_eq!(items[0].status, v1::ItemStatus::Ok as i32);
    assert_eq!(items[0].generation, row1.generation);
    match items[0].view.as_ref().unwrap() {
        v1::changeset_item::View::FilteredView(f) => {
            assert!(f.filtered);
            let paths: Vec<String> = f
                .delta
                .iter()
                .map(|d| String::from_utf8(d.path.clone()).unwrap())
                .collect();
            assert_eq!(
                paths,
                vec!["README", "bin.dat", "src/main.rs", "team/notes.txt"]
            );
            assert_eq!(f.parents, vec![idb(&r.root)]);
            assert_eq!(f.root_tree, idb(&cs1.root_tree));
            assert_eq!(f.meta, idb(&cs1.meta));
            assert_eq!(f.change_id, cs1.change_id.0.to_vec());
            // The remaining entries are exactly the readable ones of the real delta.
            let want: Vec<v1::DeltaEntryMsg> = cs1
                .delta
                .iter()
                .filter(|d| !d.path.as_bytes().starts_with(b"secret"))
                .map(v1::DeltaEntryMsg::from)
                .collect();
            assert_eq!(f.delta, want);
        }
        other => panic!("expected a filtered view, got {other:?}"),
    }
    assert_eq!(items[1].generation, row3.generation);
    assert_eq!(
        items[1].view,
        Some(v1::changeset_item::View::Encoded(row3.encoded.to_vec()))
    );

    // carol reads every delta path: the full canonical encoding, whose id is the changeset id.
    let items = get_changesets(&r.srv, &carol(), &[r.c1]).await;
    match items[0].view.as_ref().unwrap() {
        v1::changeset_item::View::Encoded(b) => {
            assert_eq!(b, &row1.encoded.to_vec());
            assert_eq!(scion_object::Changeset::decode(b).unwrap().id(), r.c1);
        }
        other => panic!("expected the full encoding, got {other:?}"),
    }

    // dave lacks repository read altogether.
    let e = r
        .srv
        .client(Some(&dave()))
        .await
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![idb(&r.c1)],
        })
        .await;
    expect_code(e, ErrorCode::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_omits_restricted_entries() {
    let r = repo().await;
    assert_eq!(
        diff_paths(&r.srv, &alice(), &r.c1, &r.c2).await,
        vec!["README", "bin.dat", "team/b.txt"]
    );
    assert_eq!(
        diff_paths(&r.srv, &carol(), &r.c1, &r.c2).await,
        vec!["README", "bin.dat", "secret/key.txt", "team/b.txt"]
    );
    // From the empty tree: everything readable in c1.
    let mut s = r
        .srv
        .client(Some(&alice()))
        .await
        .query()
        .diff(v1::DiffRequest {
            a: vec![],
            b: idb(&r.c1),
            paths: vec![],
            include_content_hunks: false,
        })
        .await
        .unwrap()
        .into_inner();
    let mut paths = Vec::new();
    while let Some(i) = s.message().await.unwrap() {
        assert!(i.old_entry.is_none() && i.new_entry.is_some());
        paths.push(String::from_utf8(i.path).unwrap());
    }
    assert_eq!(
        paths,
        vec!["README", "bin.dat", "src/main.rs", "team/notes.txt"]
    );
    // An explicitly requested restricted scope is denied; a scope that contains restricted
    // entries is filtered.
    let mut q = r.srv.client(Some(&alice())).await.query();
    for scope in ["secret", "secret/key.txt", "secret/sub"] {
        let e = q
            .diff(v1::DiffRequest {
                a: idb(&r.c1),
                b: idb(&r.c2),
                paths: vec![scope.as_bytes().to_vec()],
                include_content_hunks: false,
            })
            .await;
        expect_code(e, ErrorCode::PermissionDenied);
    }
    let mut s = q
        .diff(v1::DiffRequest {
            a: vec![],
            b: idb(&r.c2),
            paths: vec![b"".to_vec()],
            include_content_hunks: false,
        })
        .await
        .unwrap()
        .into_inner();
    while let Some(i) = s.message().await.unwrap() {
        assert!(!i.path.starts_with(b"secret"), "restricted path leaked");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_path_and_list_tree_enforce_path_read() {
    let r = repo().await;
    let mut alice_q = r.srv.client(Some(&alice())).await.query();
    for p in [
        "secret/key.txt",
        "secret",
        "secret/sub/deep.txt",
        "secret/missing",
    ] {
        let e = alice_q
            .read_path(v1::ReadPathRequest {
                changeset: idb(&r.c1),
                path: p.as_bytes().to_vec(),
            })
            .await;
        expect_code(e, ErrorCode::PermissionDenied);
    }
    for p in ["secret", "secret/sub"] {
        let e = alice_q
            .list_tree(v1::ListTreeRequest {
                changeset: idb(&r.c1),
                path: p.as_bytes().to_vec(),
            })
            .await;
        expect_code(e, ErrorCode::PermissionDenied);
    }

    // The readable root listing shows the restricted subdirectory (name and id).
    let root = alice_q
        .list_tree(v1::ListTreeRequest {
            changeset: idb(&r.c1),
            path: vec![],
        })
        .await
        .unwrap()
        .into_inner();
    let cs1 = r.ctx.history.changeset(&r.c1).await.unwrap();
    assert_eq!(root.tree_id, idb(&cs1.changeset.root_tree));
    let names: Vec<&[u8]> = root.entries.iter().map(|e| e.name.as_slice()).collect();
    assert_eq!(
        names,
        vec![b"README".as_slice(), b"bin.dat", b"secret", b"src", b"team"]
    );
    let secret_tree = r
        .ctx
        .history
        .lookup(&cs1.changeset.root_tree, &path("secret"))
        .await
        .unwrap()
        .unwrap();
    let secret_entry = root.entries[2].entry.as_ref().unwrap();
    assert_eq!(secret_entry.id, idb(&secret_tree.id));
    assert_eq!(secret_entry.mode, 4);

    // carol reads the restricted paths.
    let mut carol_q = r.srv.client(Some(&carol())).await.query();
    let mut s = carol_q
        .read_path(v1::ReadPathRequest {
            changeset: idb(&r.c2),
            path: b"secret/key.txt".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut data = Vec::new();
    while let Some(c) = s.message().await.unwrap() {
        data.extend_from_slice(&c.data);
    }
    assert_eq!(data, b"s3cr3t-2\n");
    let sub = carol_q
        .list_tree(v1::ListTreeRequest {
            changeset: idb(&r.c1),
            path: b"secret".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(sub.tree_id, idb(&secret_tree.id));
    assert_eq!(sub.entries.len(), 2);

    // dave cannot read "/" but reads /team through ReadPath and ListTree.
    let mut dave_q = r.srv.client(Some(&dave())).await.query();
    let mut s = dave_q
        .read_path(v1::ReadPathRequest {
            changeset: idb(&r.c2),
            path: b"team/notes.txt".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let first = s.message().await.unwrap().unwrap();
    assert_eq!(first.data, b"notes\n");
    let team = dave_q
        .list_tree(v1::ListTreeRequest {
            changeset: idb(&r.c2),
            path: b"team".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(team.entries.len(), 2);
    for (p, list) in [("README", false), ("", true)] {
        let e = if list {
            dave_q
                .list_tree(v1::ListTreeRequest {
                    changeset: idb(&r.c2),
                    path: p.as_bytes().to_vec(),
                })
                .await
                .map(|_| ())
        } else {
            dave_q
                .read_path(v1::ReadPathRequest {
                    changeset: idb(&r.c2),
                    path: p.as_bytes().to_vec(),
                })
                .await
                .map(|_| ())
        };
        expect_code(e, ErrorCode::PermissionDenied);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_path_on_redacted_blob_is_redacted() {
    let r = repo().await;
    let readme = r
        .ctx
        .history
        .lookup(
            &r.ctx
                .history
                .changeset(&r.c1)
                .await
                .unwrap()
                .changeset
                .root_tree,
            &path("README"),
        )
        .await
        .unwrap()
        .unwrap();
    r.ctx
        .redactor
        .redact(&[readme.id], "leaked", &admin(), None)
        .await
        .unwrap();
    r.ctx.redactions.refresh().await.unwrap();

    let mut q = r.srv.client(Some(&alice())).await.query();
    let e = q
        .read_path(v1::ReadPathRequest {
            changeset: idb(&r.c1),
            path: b"README".to_vec(),
        })
        .await;
    let err = expect_code(e, ErrorCode::Redacted);
    assert!(!err.retryable());
    // Admins get REDACTED too: redaction is not an access decision.
    let e = r
        .srv
        .client(Some(&admin()))
        .await
        .query()
        .read_path(v1::ReadPathRequest {
            changeset: idb(&r.c1),
            path: b"README".to_vec(),
        })
        .await;
    expect_code(e, ErrorCode::Redacted);

    // The tree entry still carries id and size; other files are unaffected.
    let root = q
        .list_tree(v1::ListTreeRequest {
            changeset: idb(&r.c1),
            path: vec![],
        })
        .await
        .unwrap()
        .into_inner();
    let entry = root.entries[0].entry.as_ref().unwrap();
    assert_eq!(entry.id, idb(&readme.id));
    assert_eq!(entry.size, README_1.len() as u64);
    let mut s = q
        .read_path(v1::ReadPathRequest {
            changeset: idb(&r.c1),
            path: b"src/main.rs".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(s.message().await.unwrap().unwrap().data, b"fn main() {}\n");

    // Diff with hunks over the redacted side still succeeds, without hunks for that file.
    let mut s = q
        .diff(v1::DiffRequest {
            a: idb(&r.c1),
            b: idb(&r.c2),
            paths: vec![b"README".to_vec()],
            include_content_hunks: true,
        })
        .await
        .unwrap()
        .into_inner();
    let item = s.message().await.unwrap().unwrap();
    assert_eq!(item.path, b"README");
    assert!(item.hunks.is_empty() && !item.binary);
    assert!(s.message().await.unwrap().is_none());
}

/// Two changesets with the same parents, tree, Meta and delta whose ids share their first 8
/// hex digits, found by a birthday search over change ids (SPEC 19.1).
async fn colliding_pair(r: &Repo) -> (ObjectId, ObjectId) {
    let template = (*r.ctx.history.changeset(&r.c1).await.unwrap().changeset).clone();
    let mut seen: HashMap<[u8; 4], ChangeId> = HashMap::new();
    let mut found = None;
    for n in 0u64.. {
        let mut cid = [0u8; 16];
        cid[..8].copy_from_slice(&n.to_be_bytes());
        cid[8..].copy_from_slice(b"p27-coll");
        let mut cs = template.clone();
        cs.change_id = ChangeId(cid);
        let id = cs.id();
        let mut key = [0u8; 4];
        key.copy_from_slice(&id.digest()[..4]);
        if let Some(prev) = seen.insert(key, cs.change_id) {
            found = Some((prev, cs.change_id));
            break;
        }
        assert!(n < 2_000_000, "no 32-bit prefix collision found");
    }
    let (x, y) = found.unwrap();
    let mut ids = Vec::new();
    for change_id in [x, y] {
        let mut want = template.clone();
        want.change_id = change_id;
        let created = r
            .ctx
            .history
            .create_internal(
                template.parents.clone(),
                template.delta.clone(),
                MetaSource::Existing(template.meta),
                change_id,
                &alice(),
            )
            .await
            .unwrap();
        assert_eq!(created.changeset_id, want.id());
        ids.push(created.changeset_id);
    }
    assert_eq!(ids[0].hex()[..8], ids[1].hex()[..8]);
    assert_ne!(ids[0], ids[1]);
    (ids[0], ids[1])
}

async fn resolve(
    srv: &scion_server::testing::InProcessServer,
    rev: &str,
) -> Result<ObjectId, tonic::Status> {
    srv.client(Some(&alice()))
        .await
        .query()
        .resolve(v1::ResolveRequest {
            revision: rev.to_owned(),
        })
        .await
        .map(|r| ObjectId::from_bytes(&r.into_inner().changeset).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_prefix_is_rejected_with_candidates() {
    let r = repo().await;
    let (x, y) = colliding_pair(&r).await;
    let short = x.hex()[..8].to_owned();
    for rev in [short.clone(), format!("c1{short}")] {
        let e = expect_code(resolve(&r.srv, &rev).await, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::AMBIGUOUS_PREFIX), "{rev}");
        match &e.payload {
            ErrorPayload::Candidates(c) => {
                let got: BTreeSet<ObjectId> = c.iter().copied().collect();
                assert!(got.contains(&x) && got.contains(&y), "{rev}: {c:?}");
            }
            other => panic!("expected candidates, got {other:?}"),
        }
    }
    // Longer prefixes and full ids disambiguate.
    let i = (8..64)
        .find(|&i| x.hex().as_bytes()[i] != y.hex().as_bytes()[i])
        .unwrap();
    for id in [x, y] {
        let p = &id.hex()[..=i];
        assert_eq!(resolve(&r.srv, p).await.unwrap(), id);
        assert_eq!(resolve(&r.srv, &format!("c1{p}")).await.unwrap(), id);
        assert_eq!(resolve(&r.srv, &id.to_text()).await.unwrap(), id);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revisions_resolve() {
    let r = repo().await;
    let ctx = &r.ctx;
    // Full ids, c1 prefixes and bare hex prefixes.
    for id in [r.root, r.c1, r.c2, r.c3] {
        assert_eq!(resolve(&r.srv, &id.to_text()).await.unwrap(), id);
        assert_eq!(resolve(&r.srv, &id.to_text()[..12]).await.unwrap(), id);
        assert_eq!(resolve(&r.srv, &id.hex()[..12]).await.unwrap(), id);
        assert_eq!(resolve(&r.srv, &id.hex()).await.unwrap(), id);
    }
    // Refs and ref@version.
    assert_eq!(resolve(&r.srv, "main").await.unwrap(), r.root);
    ctx.history
        .update_ref(
            UpdateRefRequest {
                name: RefName::main(),
                expected_version: 1,
                new_target: r.c2,
                allow_non_fast_forward: false,
            },
            &alice(),
            &AllowAll,
        )
        .await
        .unwrap();
    assert_eq!(resolve(&r.srv, "main").await.unwrap(), r.c2);
    assert_eq!(resolve(&r.srv, "main@1").await.unwrap(), r.root);
    assert_eq!(resolve(&r.srv, "main@2").await.unwrap(), r.c2);
    expect_code(resolve(&r.srv, "main@3").await, ErrorCode::NotFound);
    ctx.history
        .create_ref(&RefName::parse("feature/x").unwrap(), &r.c1, &admin(), "t")
        .await
        .unwrap();
    assert_eq!(resolve(&r.srv, "feature/x").await.unwrap(), r.c1);
    assert_eq!(resolve(&r.srv, "feature/x@1").await.unwrap(), r.c1);
    // An exact ref name beats a hex prefix.
    let token = r.c3.hex()[..8].to_owned();
    assert_eq!(resolve(&r.srv, &token).await.unwrap(), r.c3);
    ctx.history
        .create_ref(&RefName::parse(&token).unwrap(), &r.c1, &admin(), "t")
        .await
        .unwrap();
    assert_eq!(resolve(&r.srv, &token).await.unwrap(), r.c1);
    assert_eq!(resolve(&r.srv, &format!("c1{token}")).await.unwrap(), r.c3);

    // ~n and ^k, including a merge.
    let side = commit(ctx, &[r.c1], &[("side.txt", b"side\n")], &[]).await;
    let m = commit(ctx, &[r.c3, side], &[("merged.txt", b"m\n")], &[]).await;
    let mt = m.to_text();
    for (rev, want) in [
        (format!("{mt}~0"), m),
        (format!("{mt}~"), r.c3),
        (format!("{mt}~1"), r.c3),
        (format!("{mt}~2"), r.c2),
        (format!("{mt}~4"), r.root),
        (format!("{mt}^"), r.c3),
        (format!("{mt}^1"), r.c3),
        (format!("{mt}^2"), side),
        (format!("{mt}^2~1"), r.c1),
        (format!("{mt}^2^1~1"), r.root),
        (format!("{mt}~1^1"), r.c2),
        ("main~2".to_owned(), r.root),
        ("main@2^".to_owned(), r.c1),
    ] {
        assert_eq!(resolve(&r.srv, &rev).await.unwrap(), want, "{rev}");
    }
    for rev in [format!("{mt}~5"), format!("{mt}^3"), format!("{mt}^2^2")] {
        expect_code(resolve(&r.srv, &rev).await, ErrorCode::NotFound);
    }
    let e = expect_code(
        resolve(&r.srv, &format!("{mt}^0")).await,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(e.subcode(), Some(subcode::INVALID_REVISION));

    // git:<sha> through an import-mode changeset (eager git_mapping).
    let sha = [0xabu8; 20];
    let imported = import_commit(ctx, sha, &[("imported.txt", b"git\n")]).await;
    let sha_hex = hex::encode(sha);
    assert_eq!(
        resolve(&r.srv, &format!("git:{sha_hex}")).await.unwrap(),
        imported
    );
    expect_code(
        resolve(&r.srv, &format!("git:{}", "0".repeat(40))).await,
        ErrorCode::NotFound,
    );
    // A git suffix walk past the parentless import.
    expect_code(
        resolve(&r.srv, &format!("git:{sha_hex}~1")).await,
        ErrorCode::NotFound,
    );
}
