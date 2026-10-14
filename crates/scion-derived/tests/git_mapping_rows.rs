//! SPEC 9.2 / 14 `git_mapping` (implementation-defined 6.4): rows exist in both directions only
//! for import-mode creations whose Meta carries extension tag 2; a non-import changeset with
//! tag 2 creates no mapping; a re-commit in import mode fills in missing mapping rows. Tested
//! through a mock `DeriveCtx` and end to end through `History::create_changeset` with
//! `DerivedEngine` (`Registry::v1_default`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "history_support/mod.rs"]
mod history_support;

use bytes::Bytes;
use history_support::*;
use scion_core::{ErrorCode, Kind, ObjectId, subcode};
use scion_derived::git_mapping::{self, GitMappingType};
use scion_derived::{DeriveMode, DerivedType, EagerType};
use scion_history::{AllowAll, CreateRequest, EagerInput};
use scion_object::{Canonical, Extensions, META_EXT_GIT_COMMIT};
use scion_refmodel::MTree;

fn sha(n: u8) -> [u8; 20] {
    let mut s = [0u8; 20];
    for (i, b) in s.iter_mut().enumerate() {
        *b = n.wrapping_mul(31).wrapping_add(i as u8 * 7);
    }
    s
}

fn tag2(sha: &[u8]) -> Extensions {
    let mut e = Extensions::new();
    e.insert(META_EXT_GIT_COMMIT, sha.to_vec()).unwrap();
    e
}

async fn derive_mock(
    import_mode: bool,
    ext: Extensions,
) -> (ObjectId, Vec<scion_store::DerivedRow>) {
    let cx = MockCtx::new();
    let cs = changeset(Vec::new(), empty_tree(), Vec::new(), 1);
    let id = cs.id();
    let meta = sample_meta(ext);
    let input = EagerInput {
        id: &id,
        changeset: &cs,
        meta: &meta,
        new_trees: &[],
        import_mode,
    };
    (
        id,
        GitMappingType.derive(cx.as_ref(), &input).await.unwrap(),
    )
}

#[test]
fn descriptor() {
    assert_eq!(GitMappingType.name(), "git_mapping");
    assert_eq!(GitMappingType.version(), 1);
    assert_eq!(GitMappingType.mode(), DeriveMode::Eager);
    assert!(!GitMappingType.content_bearing());
}

#[tokio::test]
async fn mock_rows_only_in_import_mode_with_tag_2() {
    let s = sha(1);
    let hex = hex::encode(s);
    let (id, rows) = derive_mock(true, tag2(&s)).await;
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.key.ty, "git_mapping");
        assert_eq!(r.key.version, 1);
        assert!(r.source_object.is_none());
        assert!(r.storage_key.is_none());
    }
    assert_eq!(&rows[0].key.key[..], format!("g:{hex}").as_bytes());
    assert_eq!(rows[0].value.as_deref(), Some(&id.as_bytes()[..]));
    let mut ck = b"c:".to_vec();
    ck.extend_from_slice(id.as_bytes());
    assert_eq!(&rows[1].key.key[..], &ck[..]);
    assert_eq!(rows[1].value.as_deref(), Some(hex.as_bytes()));
    // Deterministic.
    assert_eq!(derive_mock(true, tag2(&s)).await.1, rows);

    // No import mode: no rows even with tag 2.
    assert!(derive_mock(false, tag2(&s)).await.1.is_empty());
    // Import mode without tag 2, or with a malformed tag 2: no rows.
    assert!(derive_mock(true, Extensions::new()).await.1.is_empty());
    assert!(derive_mock(true, tag2(&[1, 2, 3])).await.1.is_empty());
    assert!(derive_mock(true, tag2(&[0u8; 32])).await.1.is_empty());
}

#[tokio::test]
async fn import_mode_creation_maps_both_directions() {
    let st = stack();
    let root = st.h.bootstrap().await.unwrap();
    let t1 = tree(&[("README", "hello\n")]);
    let s1 = sha(2);
    let hex = hex::encode(s1);
    let c1 = st
        .commit_tree(vec![root], &MTree::new(), &t1, 1, true, tag2(&s1))
        .await
        .unwrap();
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex)
            .await
            .unwrap(),
        Some(c1)
    );
    // Lookups are case-insensitive on the hex form.
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex.to_ascii_uppercase())
            .await
            .unwrap(),
        Some(c1)
    );
    assert_eq!(
        git_mapping::git_for_changeset(&st.engine, &c1)
            .await
            .unwrap(),
        Some(hex.clone())
    );
    // A parentless import commit with tag 2 is mapped as well.
    let s2 = sha(3);
    let c2 = st
        .commit_tree(Vec::new(), &MTree::new(), &t1, 2, true, tag2(&s2))
        .await
        .unwrap();
    assert_eq!(
        git_mapping::git_for_changeset(&st.engine, &c2)
            .await
            .unwrap(),
        Some(hex::encode(s2))
    );
    // The root has no mapping; unknown ids map to nothing.
    assert_eq!(
        git_mapping::git_for_changeset(&st.engine, &root)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex::encode(sha(9)))
            .await
            .unwrap(),
        None
    );
    let e = git_mapping::changeset_for_git(&st.engine, "not-a-sha")
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT));
    assert!(st.fsck().await.is_empty());
}

#[tokio::test]
async fn non_import_changeset_with_tag_2_creates_no_mapping() {
    let st = stack();
    let root = st.h.bootstrap().await.unwrap();
    let s1 = sha(4);
    let c1 = st
        .commit_tree(
            vec![root],
            &MTree::new(),
            &tree(&[("a", "1\n")]),
            1,
            false,
            tag2(&s1),
        )
        .await
        .unwrap();
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex::encode(s1))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        git_mapping::git_for_changeset(&st.engine, &c1)
            .await
            .unwrap(),
        None
    );
    // No git_mapping rows at all were written.
    let rows = st.mem.meta.table_counts().derived;
    // history rows only: root + c1.
    assert_eq!(rows, 2);
    assert!(st.fsck().await.is_empty());
}

#[tokio::test]
async fn re_commit_in_import_mode_fills_in_missing_mapping_rows() {
    let st = stack();
    let root = st.h.bootstrap().await.unwrap();
    let s1 = sha(5);
    let hex = hex::encode(s1);
    let t1 = tree(&[("lib/x", "x\n")]);
    st.upload(&t1).await;
    let req = |import_mode: bool| CreateRequest {
        parents: vec![root],
        delta: delta_of(&MTree::new(), &t1),
        meta: fields("imported", tag2(&s1)),
        change_id: change_id(1),
        expected_root_tree: None,
        import_mode,
    };
    // The same principal both times, so the Meta and the changeset id are identical.
    let who = importer();
    let first =
        st.h.create_changeset(req(false), &who, &AllowAll)
            .await
            .unwrap();
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex)
            .await
            .unwrap(),
        None
    );
    let again =
        st.h.create_changeset(req(true), &who, &AllowAll)
            .await
            .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        git_mapping::changeset_for_git(&st.engine, &hex)
            .await
            .unwrap(),
        Some(first.changeset_id)
    );
    assert_eq!(
        git_mapping::git_for_changeset(&st.engine, &first.changeset_id)
            .await
            .unwrap(),
        Some(hex.clone())
    );
    // A third, identical import-mode commit is idempotent.
    let third =
        st.h.create_changeset(req(true), &who, &AllowAll)
            .await
            .unwrap();
    assert_eq!(third, first);
    assert_eq!(st.mem.meta.table_counts().changesets, 2);
    assert!(st.fsck().await.is_empty());

    // The stored values are exactly the documented encodings.
    let v = st
        .engine
        .get_stored(
            git_mapping::NAME,
            Some(git_mapping::VERSION),
            &git_mapping::git_key(&hex),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v, Bytes::copy_from_slice(first.changeset_id.as_bytes()));
    assert_eq!(first.changeset_id.kind(), Kind::Changeset);
}
