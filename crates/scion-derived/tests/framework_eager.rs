//! SPEC 19.6 / 13.5 framework items (P14): the EagerHook output, required eager pairs,
//! purge of content-bearing records, and the StoreDeriveCtx contract.

mod framework_support;

use async_trait::async_trait;
use bytes::Bytes;
use framework_support::*;
use scion_core::{ErrorCode, Kind, ObjectId, Result};
use scion_derived::{DeriveCtx, DeriveMode, DerivedType, LazyType, Registry, StoreDeriveCtx};
use scion_history::{EagerHook, EagerInput};
use scion_object::{Canonical, Tree};
use scion_store::{AuditRow, ChangesetBundle, ChangesetRow, DerivedKey, DerivedRow, RedactTxn};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A content-bearing lazy type keyed by a blob id: the blob's length (redacted reads as 0).
struct BlobLenType;

impl DerivedType for BlobLenType {
    fn name(&self) -> &'static str {
        "test_blob_len"
    }
    fn version(&self) -> u32 {
        1
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }
    fn content_bearing(&self) -> bool {
        true
    }
}

#[async_trait]
impl LazyType for BlobLenType {
    async fn deps(&self, _cx: &dyn DeriveCtx, _key: &[u8]) -> Result<Vec<Bytes>> {
        Ok(vec![])
    }
    async fn derive(
        &self,
        cx: &dyn DeriveCtx,
        key: &[u8],
        _deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        let id = ObjectId::from_bytes(key)?;
        let b = cx.blob_or_empty(&id).await?;
        Ok(Bytes::copy_from_slice(&(b.len() as u64).to_be_bytes()))
    }
}

fn row(ty: &str, key: &[u8], value: &[u8], source: Option<ObjectId>) -> DerivedRow {
    DerivedRow {
        key: DerivedKey {
            ty: ty.to_owned(),
            version: 1,
            key: Bytes::copy_from_slice(key),
        },
        value: Some(Bytes::copy_from_slice(value)),
        storage_key: None,
        source_object: source,
        at: SystemTime::UNIX_EPOCH,
    }
}

fn redact_txn(id: ObjectId) -> RedactTxn {
    RedactTxn {
        ids: vec![id],
        reason: "test".into(),
        actor: "github:admin".into(),
        at: SystemTime::UNIX_EPOCH,
        purge_after: SystemTime::UNIX_EPOCH + Duration::from_secs(86_400),
    }
}

fn audit() -> AuditRow {
    AuditRow {
        at: SystemTime::UNIX_EPOCH,
        actor: "github:admin".into(),
        action: "purge".into(),
        target: String::new(),
        reason: "test".into(),
        detail: String::new(),
    }
}

#[tokio::test]
async fn eager_hook_concatenates_all_registered_eager_types() {
    let (storage, _h) = storage();
    let mut reg = Registry::empty();
    reg.register_eager(Arc::new(TagEager {
        name: "test_e1",
        version: 1,
        extra: 2,
        content_bearing: false,
    }));
    reg.register_lazy(Arc::new(ChainType::new("test_lazy", 1)));
    reg.register_eager(Arc::new(TagEager {
        name: "test_e2",
        version: 3,
        extra: 0,
        content_bearing: true,
    }));
    reg.register_descriptor("test_desc", 1, DeriveMode::Eager, false);
    let eng = engine(&storage, reg, "w");
    let m = Arc::new(RecMetrics::default());
    eng.set_metrics(m.clone());

    let (id, cs, meta) = sample_changeset();
    let input = EagerInput {
        id: &id,
        changeset: &cs,
        meta: &meta,
        new_trees: &[],
        import_mode: false,
    };
    let rows = eng.derive_eager(&input).await.unwrap();
    let keys: Vec<(String, u32, Bytes)> = rows
        .iter()
        .map(|r| (r.key.ty.clone(), r.key.version, r.key.key.clone()))
        .collect();
    let idb = Bytes::copy_from_slice(id.as_bytes());
    assert_eq!(
        keys,
        vec![
            ("test_e1".to_owned(), 1, idb.clone()),
            ("test_e1".to_owned(), 1, Bytes::from_static(b"test_e1:0")),
            ("test_e1".to_owned(), 1, Bytes::from_static(b"test_e1:1")),
            ("test_e2".to_owned(), 3, idb),
        ]
    );
    // Deterministic: a second run yields identical rows.
    assert_eq!(eng.derive_eager(&input).await.unwrap(), rows);
    assert_eq!(m.durations_of("test_e1", DeriveMode::Eager), 2);
    assert_eq!(m.durations_of("test_e2", DeriveMode::Eager), 2);

    let req = eng.required_eager();
    assert_eq!(
        req,
        vec![
            ("test_e1".to_owned(), 1),
            ("test_e2".to_owned(), 3),
            ("test_desc".to_owned(), 1)
        ]
    );
}

#[tokio::test]
async fn empty_registry_eager_hook_emits_nothing() {
    let (storage, _h) = storage();
    let eng = engine(&storage, Registry::empty(), "w");
    let (id, cs, meta) = sample_changeset();
    let input = EagerInput {
        id: &id,
        changeset: &cs,
        meta: &meta,
        new_trees: &[],
        import_mode: true,
    };
    assert!(eng.derive_eager(&input).await.unwrap().is_empty());
    assert!(eng.required_eager().is_empty());
}

#[tokio::test]
async fn v1_default_required_eager() {
    let (storage, _h) = storage();
    let eng = engine(&storage, Registry::v1_default(), "w");
    let req = eng.required_eager();
    for name in ["history", "git_mapping", "commit_graph", "path_index"] {
        assert!(req.contains(&(name.to_owned(), 1)), "{name}");
    }
    assert_eq!(req.len(), 4);
}

#[tokio::test]
async fn purge_content_for_deletes_only_content_bearing_records_of_the_id() {
    let (storage, handles) = storage();
    let repo = repo(&storage);
    let (x, _) = repo
        .put_blob(Bytes::from_static(b"secret\n"))
        .await
        .unwrap();
    let (y, _) = repo.put_blob(Bytes::from_static(b"other\n")).await.unwrap();

    let mut reg = Registry::empty();
    reg.register_eager(Arc::new(TagEager {
        name: "test_content",
        version: 1,
        extra: 0,
        content_bearing: true,
    }));
    reg.register_eager(Arc::new(TagEager {
        name: "test_plain",
        version: 1,
        extra: 0,
        content_bearing: false,
    }));
    reg.register_lazy(Arc::new(BlobLenType));
    let eng = engine(&storage, reg, "w");
    assert_eq!(
        eng.registry().content_bearing_types(),
        vec!["test_blob_len".to_owned(), "test_content".to_owned()]
    );

    storage
        .meta
        .put_derived(vec![
            row("test_content", b"a", b"x-content", Some(x)),
            row("test_content", b"b", b"y-content", Some(y)),
            row("test_plain", b"c", b"x-plain", Some(x)),
            row("test_plain", b"d", b"plain", None),
        ])
        .await
        .unwrap();
    // A content-bearing lazy write-back is keyed by its source id.
    let len = eng.get("test_blob_len", None, x.as_bytes()).await.unwrap();
    assert_eq!(len.as_ref(), &7u64.to_be_bytes());
    eng.get("test_blob_len", None, y.as_bytes()).await.unwrap();
    assert_eq!(handles.meta.table_counts().derived, 6);

    assert_eq!(eng.purge_content_for(&x).await.unwrap(), 2);
    assert_eq!(handles.meta.table_counts().derived, 4);
    assert_eq!(
        eng.get_stored("test_content", None, b"a").await.unwrap(),
        None
    );
    assert_eq!(
        eng.get_stored("test_blob_len", None, x.as_bytes())
            .await
            .unwrap(),
        None
    );
    assert!(
        eng.get_stored("test_content", None, b"b")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        eng.get_stored("test_plain", None, b"c")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        eng.get_stored("test_plain", None, b"d")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        eng.get_stored("test_blob_len", None, y.as_bytes())
            .await
            .unwrap()
            .is_some()
    );
    // Idempotent.
    assert_eq!(eng.purge_content_for(&x).await.unwrap(), 0);

    // After redaction, a re-derivation treats the content as empty (SPEC 13.5).
    storage.meta.redact(redact_txn(x)).await.unwrap();
    let len = eng.get("test_blob_len", None, x.as_bytes()).await.unwrap();
    assert_eq!(len.as_ref(), &0u64.to_be_bytes());

    // A registry without content-bearing types deletes nothing.
    let plain = engine(&storage, Registry::v1_default(), "p");
    assert_eq!(plain.purge_content_for(&y).await.unwrap(), 0);
}

#[tokio::test]
async fn store_ctx_blobs_redacted_and_purged_read_empty() {
    let (storage, _h) = storage();
    let repo = repo(&storage);
    let ctx = StoreDeriveCtx::new(repo.clone());
    let (live, _) = repo.put_blob(Bytes::from_static(b"live\n")).await.unwrap();
    let (q, _) = repo
        .put_blob(Bytes::from_static(b"quarantined\n"))
        .await
        .unwrap();
    let (p, _) = repo
        .put_blob(Bytes::from_static(b"purged\n"))
        .await
        .unwrap();

    assert_eq!(ctx.blob_or_empty(&live).await.unwrap().as_ref(), b"live\n");
    assert_eq!(ctx.get_object(&q).await.unwrap().as_ref(), b"quarantined\n");

    storage.meta.redact(redact_txn(q)).await.unwrap();
    repo.quarantine_bytes(&q).await.unwrap();
    storage.meta.redact(redact_txn(p)).await.unwrap();
    storage.meta.mark_purged(&[p], audit()).await.unwrap();
    repo.purge_bytes(&p).await.unwrap();

    assert!(ctx.blob_or_empty(&q).await.unwrap().is_empty());
    assert!(ctx.blob_or_empty(&p).await.unwrap().is_empty());
    assert_eq!(ctx.blob_or_empty(&live).await.unwrap().as_ref(), b"live\n");
    // get_object does not hide redaction.
    assert_eq!(
        ctx.get_object(&q).await.unwrap_err().code,
        ErrorCode::Redacted
    );
    // A missing blob is still NOT_FOUND (not silently empty).
    let missing = ObjectId::hash_blob(b"never stored");
    assert_eq!(
        ctx.blob_or_empty(&missing).await.unwrap_err().code,
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn store_ctx_meta_none_when_redacted() {
    let (storage, _h) = storage();
    let repo = repo(&storage);
    let ctx = StoreDeriveCtx::new(repo.clone());
    let (_, _, meta) = sample_changeset();
    let id = ctx
        .put_object(Kind::Meta, Bytes::from(meta.encode()))
        .await
        .unwrap();
    assert_eq!(id, meta.id());
    let got = ctx.meta(&id).await.unwrap().unwrap();
    assert_eq!(*got, meta);
    storage.meta.redact(redact_txn(id)).await.unwrap();
    assert!(ctx.meta(&id).await.unwrap().is_none());
    storage.meta.mark_purged(&[id], audit()).await.unwrap();
    assert!(ctx.meta(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn store_ctx_trees_changesets_and_stored_values() {
    let (storage, _h) = storage();
    let repo = repo(&storage);
    let ctx = StoreDeriveCtx::new(repo.clone());

    // put_object goes through ObjectRepo::put_encoded (validated, inline).
    let tree = Tree::empty();
    let tid = ctx
        .put_object(Kind::Tree, Bytes::from(tree.encode()))
        .await
        .unwrap();
    assert_eq!(tid, tree.id());
    assert_eq!(*ctx.tree(&tid).await.unwrap(), tree);
    assert!(
        ctx.put_object(Kind::Tree, Bytes::from_static(b"\xff\xff"))
            .await
            .is_err()
    );

    // A changeset with its graph columns.
    let (cid, cs, meta) = sample_changeset();
    ctx.put_object(Kind::Meta, Bytes::from(meta.encode()))
        .await
        .unwrap();
    let crow = ChangesetRow {
        id: cid,
        encoded: Bytes::from(cs.encode()),
        generation: 1,
        skip: vec![],
        change_id: cs.change_id,
        created_by: "github:alice".into(),
        created_at: SystemTime::UNIX_EPOCH,
    };
    storage
        .meta
        .commit_changeset(ChangesetBundle {
            row: crow,
            parents: vec![],
            object_paths: vec![],
            derived: vec![],
        })
        .await
        .unwrap();
    let rec = ctx.changeset(&cid).await.unwrap();
    assert_eq!(rec.id, cid);
    assert_eq!(*rec.changeset, cs);
    assert_eq!(rec.generation, 1);
    assert!(rec.skip.is_empty());
    assert_eq!(rec.created_by, "github:alice");

    // Stored derived values: inline, out-of-line (storage_key), absent.
    storage
        .meta
        .put_derived(vec![row("test_t", b"inline", b"v1", None)])
        .await
        .unwrap();
    storage
        .blobs
        .put("derived/test_t/big", Bytes::from_static(b"big value"))
        .await
        .unwrap();
    let mut big = row("test_t", b"big", b"", None);
    big.value = None;
    big.storage_key = Some("derived/test_t/big".into());
    storage.meta.put_derived(vec![big]).await.unwrap();
    let k = |key: &'static [u8]| DerivedKey {
        ty: "test_t".into(),
        version: 1,
        key: Bytes::from_static(key),
    };
    assert_eq!(
        ctx.stored(&k(b"inline")).await.unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(
        ctx.stored(&k(b"big")).await.unwrap().as_deref(),
        Some(&b"big value"[..])
    );
    assert_eq!(ctx.stored(&k(b"absent")).await.unwrap(), None);
}
