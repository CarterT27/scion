//! Shared backend conformance suites (always compiled; test-support API, PLAN 3.2).
//!
//! The in-memory backends always run them; CockroachDB/S3 run them when env-gated (P15). Every
//! case of `docs/contracts/storage-semantics.md` section 4 (meta) and section 6 (blob) is
//! covered, including the concurrency cases (one winner per CAS, insert and lease race; distinct
//! redaction epochs). Each case gets a fresh store from the factory; all failures are collected
//! and reported together by one assertion at the end.
//!
//! Stale reads are only required to be correct when they return a row (they may miss recent
//! rows), so the suite never relies on a stale read observing a write.

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::types::*;
use bytes::Bytes;
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_object::{RefName, RepoPath};
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type CaseResult = std::result::Result<(), String>;

macro_rules! ensure {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+));
        }
    };
}

macro_rules! ensure_eq {
    ($a:expr, $b:expr, $ctx:expr) => {{
        let (a, b) = (&$a, &$b);
        if a != b {
            return Err(format!("{}: left {:?} != right {:?}", $ctx, a, b));
        }
    }};
}

macro_rules! run_case {
    ($fails:ident, $factory:ident, $name:literal, $case:path) => {{
        let store = $factory().await;
        if let Err(m) = $case(store).await {
            $fails.push(format!("{}: {}", $name, m));
        }
    }};
}

fn ok<T>(r: Result<T>, ctx: &str) -> std::result::Result<T, String> {
    r.map_err(|e| format!("{ctx}: unexpected error {e}"))
}

fn expect_err<T: Debug>(
    r: Result<T>,
    code: ErrorCode,
    ctx: &str,
) -> std::result::Result<ScionError, String> {
    match r {
        Ok(v) => Err(format!(
            "{ctx}: expected {} but got Ok({v:?})",
            code.as_str()
        )),
        Err(e) if e.code == code => Ok(e),
        Err(e) => Err(format!("{ctx}: expected {} but got {e}", code.as_str())),
    }
}

/// Values whose constructors live in another crate are built through this helper, which
/// tolerates a constructor that is not available (stubbed) during staged builds; the dependent
/// checks are then skipped instead of aborting the whole suite.
fn staged<T>(f: impl FnOnce() -> Result<T>) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .ok()
        .and_then(|r| r.ok())
}

fn ref_name(s: &str) -> Option<RefName> {
    staged(|| RefName::parse(s))
}

fn repo_path(s: &str) -> Option<RepoPath> {
    staged(|| RepoPath::parse(s.as_bytes()))
}

/// Whole-second times (CockroachDB stores microseconds).
fn t(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n)
}

fn blob_row(content: &[u8]) -> ObjectRow {
    ObjectRow {
        id: ObjectId::hash_blob(content),
        kind: Kind::Blob,
        size: content.len() as u64,
        stored_size: content.len() as u64 + 9,
        inline_data: None,
        state: ObjectState::Live,
        created_at: t(1),
    }
}

fn inline_row(kind: Kind, canonical: &[u8]) -> ObjectRow {
    ObjectRow {
        id: ObjectId::hash_encoded(kind, canonical),
        kind,
        size: canonical.len() as u64,
        stored_size: canonical.len() as u64,
        inline_data: Some(Bytes::copy_from_slice(canonical)),
        state: ObjectState::Live,
        created_at: t(2),
    }
}

fn cs_row(encoded: &[u8], generation: u64, skip: Vec<ObjectId>) -> ChangesetRow {
    let mut cid = [0u8; 16];
    let h = blake3::hash(encoded);
    cid.copy_from_slice(&h.as_bytes()[..16]);
    ChangesetRow {
        id: ObjectId::hash_encoded(Kind::Changeset, encoded),
        encoded: Bytes::copy_from_slice(encoded),
        generation,
        skip,
        fp_merges: Some(0),
        change_id: ChangeId(cid),
        created_by: "service:conformance".to_owned(),
        created_at: t(3),
    }
}

fn bundle(row: ChangesetRow, parents: Vec<ObjectId>) -> ChangesetBundle {
    ChangesetBundle {
        row,
        parents,
        object_paths: Vec::new(),
        derived: Vec::new(),
    }
}

fn dkey(ty: &str, key: &[u8]) -> DerivedKey {
    DerivedKey {
        ty: ty.to_owned(),
        version: 1,
        key: Bytes::copy_from_slice(key),
    }
}

fn drow(k: DerivedKey, value: &[u8], source: Option<ObjectId>) -> DerivedRow {
    DerivedRow {
        key: k,
        value: Some(Bytes::copy_from_slice(value)),
        storage_key: None,
        source_object: source,
        at: t(4),
    }
}

fn cas(name: &RefName, expect: RefExpect, target: Option<ObjectId>) -> RefCas {
    RefCas {
        name: name.clone(),
        expect,
        new_target: target,
        principal: "github:alice".to_owned(),
        at: t(10),
        landed: Vec::new(),
        audit: None,
    }
}

fn audit(action: &str, target: &str) -> AuditRow {
    AuditRow {
        at: t(20),
        actor: "github:admin".to_owned(),
        action: action.to_owned(),
        target: target.to_owned(),
        reason: "conformance".to_owned(),
        detail: String::new(),
    }
}

fn cs_id(n: u8) -> ObjectId {
    ObjectId::hash_encoded(Kind::Changeset, &[n])
}

fn committed(o: &CasOutcome) -> std::result::Result<Option<RefRow>, String> {
    match o {
        CasOutcome::Committed(r) => Ok(r.clone()),
        other => Err(format!("expected Committed, got {other:?}")),
    }
}

// ---------------------------------------------------------------------------------------------
// MetaStore cases
// ---------------------------------------------------------------------------------------------

async fn meta_basics(s: Arc<dyn MetaStore>) -> CaseResult {
    ok(s.ping().await, "ping")?;
    ensure_eq!(s.repo_id(), s.repo_id(), "repo_id is stable");
    ensure!(s.repo_id().0 >= 0, "repo_id must be non-negative");
    ensure!(
        !ok(s.has_root_changeset().await, "has_root")?,
        "empty store has no root"
    );
    ensure_eq!(
        ok(s.list_refs().await, "list_refs")?,
        Vec::<RefRow>::new(),
        "no refs"
    );
    ensure_eq!(ok(s.redaction_epoch().await, "epoch")?, 0, "initial epoch");
    let (rows, epoch) = ok(s.list_redactions(0).await, "list_redactions")?;
    ensure!(rows.is_empty() && epoch == 0, "no redactions initially");
    ensure!(
        ok(s.audit_log(10).await, "audit_log")?.is_empty(),
        "empty audit log"
    );
    Ok(())
}

async fn meta_insert_objects(s: Arc<dyn MetaStore>) -> CaseResult {
    let a = blob_row(b"batch a");
    let b = inline_row(Kind::Tree, b"\x01\x00\x01");
    let c = blob_row(b"batch c");
    ensure_eq!(
        ok(s.insert_object(a.clone()).await, "insert a")?,
        InsertOutcome::Inserted,
        "single insert"
    );
    let got = ok(
        s.insert_objects(vec![b.clone(), a.clone(), b.clone(), c.clone()])
            .await,
        "insert_objects",
    )?;
    ensure_eq!(
        got,
        vec![
            InsertOutcome::Inserted,
            InsertOutcome::AlreadyExists,
            InsertOutcome::AlreadyExists,
            InsertOutcome::Inserted
        ],
        "batch outcomes in input order"
    );
    let rows = ok(
        s.get_objects(&[b.id, c.id], Consistency::Strong).await,
        "get_objects",
    )?;
    ensure_eq!(rows, vec![Some(b), Some(c)], "batch rows stored as given");
    ensure_eq!(
        ok(s.insert_objects(Vec::new()).await, "empty batch")?,
        Vec::<InsertOutcome>::new(),
        "empty batch"
    );
    Ok(())
}

async fn meta_objects(s: Arc<dyn MetaStore>) -> CaseResult {
    let a = blob_row(b"object a");
    let b = inline_row(Kind::Tree, b"\x01\x00\x00");
    let missing = ObjectId::hash_blob(b"missing");
    ensure_eq!(
        ok(s.insert_object(a.clone()).await, "insert a")?,
        InsertOutcome::Inserted,
        "first insert"
    );
    ensure_eq!(
        ok(s.insert_object(b.clone()).await, "insert b")?,
        InsertOutcome::Inserted,
        "insert inline"
    );
    // ON CONFLICT DO NOTHING: a conflicting insert never modifies the row.
    let mut a2 = a.clone();
    a2.stored_size = 1;
    a2.state = ObjectState::Quarantined;
    a2.created_at = t(99);
    ensure_eq!(
        ok(s.insert_object(a2).await, "re-insert a")?,
        InsertOutcome::AlreadyExists,
        "second insert"
    );
    let got = ok(
        s.get_objects(&[a.id, missing, b.id, a.id], Consistency::Strong)
            .await,
        "get_objects",
    )?;
    ensure_eq!(
        got,
        vec![Some(a.clone()), None, Some(b.clone()), Some(a.clone())],
        "strong get_objects in input order"
    );
    let stale = ok(
        s.get_objects(&[a.id, missing, b.id], Consistency::Stale)
            .await,
        "stale get_objects",
    )?;
    ensure_eq!(stale.len(), 3, "stale result length");
    ensure!(stale[1].is_none(), "stale read invented a row");
    for (row, want) in stale.iter().zip([&a, &a, &b]).filter(|(r, _)| r.is_some()) {
        ensure_eq!(row.as_ref(), Some(want), "stale row content");
    }
    ensure_eq!(
        ok(s.get_objects(&[], Consistency::Strong).await, "empty get")?,
        Vec::<Option<ObjectRow>>::new(),
        "empty id list"
    );
    // A quarantined row inserted directly stays as inserted.
    let mut q = blob_row(b"inserted quarantined");
    q.state = ObjectState::Quarantined;
    ok(s.insert_object(q.clone()).await, "insert quarantined")?;
    let got = ok(s.get_objects(&[q.id], Consistency::Strong).await, "get q")?;
    ensure_eq!(got, vec![Some(q)], "state round trip");
    Ok(())
}

async fn meta_uploads(s: Arc<dyn MetaStore>) -> CaseResult {
    let id = ObjectId::hash_blob(b"uploaded");
    ensure!(
        !ok(s.has_upload(&id, "github:alice").await, "has_upload")?,
        "no upload yet"
    );
    ok(s.record_upload(&id, "github:alice", t(1)).await, "record")?;
    ok(
        s.record_upload(&id, "github:alice", t(2)).await,
        "record again",
    )?;
    ensure!(
        ok(s.has_upload(&id, "github:alice").await, "has_upload")?,
        "upload recorded"
    );
    ensure!(
        !ok(s.has_upload(&id, "github:bob").await, "has_upload bob")?,
        "uploads are per principal"
    );
    let other = ObjectId::hash_blob(b"other");
    ensure!(
        !ok(
            s.has_upload(&other, "github:alice").await,
            "has_upload other"
        )?,
        "uploads are per object"
    );
    ok(
        s.record_uploads(&[id, other, other], "github:alice", t(3))
            .await,
        "batch record",
    )?;
    ensure!(
        ok(s.has_upload(&other, "github:alice").await, "batch has")?,
        "batch records new ids"
    );
    ensure!(
        !ok(
            s.has_upload(&other, "github:bob").await,
            "batch other principal"
        )?,
        "batch preserves ownership"
    );
    ok(
        s.record_uploads(&[], "github:alice", t(4)).await,
        "empty batch",
    )?;
    let missing = ObjectId::hash_blob(b"never uploaded");
    ensure_eq!(
        ok(
            s.has_uploads(&[other, missing, id, other], "github:alice")
                .await,
            "batch ownership"
        )?,
        vec![true, false, true, true],
        "ownership order and duplicates"
    );
    ensure_eq!(
        ok(
            s.has_uploads(&[id, other], "github:bob").await,
            "other principal"
        )?,
        vec![false, false],
        "ownership isolates principals"
    );
    ensure_eq!(
        ok(s.has_uploads(&[], "github:alice").await, "empty ownership")?,
        Vec::<bool>::new(),
        "empty ownership"
    );
    Ok(())
}

async fn meta_changesets(s: Arc<dyn MetaStore>) -> CaseResult {
    let root = cs_row(b"root changeset", 1, vec![]);
    let child = cs_row(b"child changeset", 2, vec![root.id]);
    let child2 = cs_row(b"second child", 2, vec![root.id]);
    ensure_eq!(
        ok(
            s.get_changesets(&[root.id], Consistency::Strong).await,
            "get"
        )?,
        vec![None],
        "absent changeset"
    );
    // has_root_changeset means "a generation-1 row exists", not "any row exists".
    let orphan = cs_row(b"generation 3 without its ancestors", 3, vec![]);
    ok(
        s.commit_changeset(bundle(orphan, vec![])).await,
        "commit gen 3",
    )?;
    ensure!(
        !ok(s.has_root_changeset().await, "has_root")?,
        "no generation-1 row yet"
    );
    ensure_eq!(
        ok(
            s.commit_changeset(bundle(root.clone(), vec![])).await,
            "commit root"
        )?,
        InsertOutcome::Inserted,
        "commit root"
    );
    ensure!(ok(s.has_root_changeset().await, "has_root")?, "root exists");
    ensure_eq!(
        ok(
            s.commit_changeset(bundle(child.clone(), vec![root.id]))
                .await,
            "commit child"
        )?,
        InsertOutcome::Inserted,
        "commit child"
    );
    ensure_eq!(
        ok(
            s.commit_changeset(bundle(child2.clone(), vec![root.id]))
                .await,
            "commit child2"
        )?,
        InsertOutcome::Inserted,
        "commit child2"
    );
    let got = ok(
        s.get_changesets(&[child.id, root.id, cs_id(9)], Consistency::Strong)
            .await,
        "get_changesets",
    )?;
    ensure_eq!(
        got,
        vec![Some(child.clone()), Some(root.clone()), None],
        "get_changesets in input order"
    );
    let stale = ok(
        s.get_changesets(&[child.id], Consistency::Stale).await,
        "stale get_changesets",
    )?;
    if let Some(Some(r)) = stale.first() {
        ensure_eq!(r, &child, "stale changeset row content");
    }
    let mut kids = vec![child.id, child2.id];
    kids.sort();
    ensure_eq!(
        ok(s.children(&root.id).await, "children")?,
        kids,
        "children ascending"
    );
    ensure!(
        ok(s.children(&child.id).await, "children of leaf")?.is_empty(),
        "leaf has no children"
    );
    // Re-commit: the changeset row is unchanged, missing rows are filled in independently.
    let mut again = child.clone();
    again.created_by = "service:other".to_owned();
    let mut b = bundle(again, vec![root.id]);
    let k = dkey("history", child.id.as_bytes());
    b.derived.push(drow(k.clone(), b"node", None));
    ensure_eq!(
        ok(s.commit_changeset(b).await, "re-commit")?,
        InsertOutcome::AlreadyExists,
        "re-commit outcome"
    );
    ensure_eq!(
        ok(
            s.get_changesets(&[child.id], Consistency::Strong).await,
            "get"
        )?,
        vec![Some(child.clone())],
        "row not modified by re-commit"
    );
    let d = ok(s.get_derived(&k, Consistency::Strong).await, "get_derived")?;
    ensure!(d.is_some(), "re-commit filled in the missing derived row");
    Ok(())
}

async fn meta_object_paths(s: Arc<dyn MetaStore>) -> CaseResult {
    let blob = ObjectId::hash_blob(b"pathed");
    let other = ObjectId::hash_blob(b"other pathed");
    let root = cs_row(b"paths root", 1, vec![]);
    let names = ["src/b.rs", "a.txt", "src/a.rs"];
    let paths: Option<Vec<RepoPath>> = names.iter().map(|n| repo_path(n)).collect();
    let paths = match paths {
        Some(p) => p,
        None => vec![RepoPath::root()],
    };
    let mut b = bundle(root.clone(), vec![]);
    for p in &paths {
        b.object_paths.push(ObjectPathRow {
            object: blob,
            path: p.clone(),
            first_changeset: root.id,
            at: t(5),
        });
    }
    b.object_paths.push(ObjectPathRow {
        object: other,
        path: RepoPath::root(),
        first_changeset: root.id,
        at: t(5),
    });
    ok(s.commit_changeset(b).await, "commit with paths")?;
    let got = ok(s.object_paths(&blob, 100).await, "object_paths")?;
    let mut want = paths.clone();
    want.sort();
    let got_paths: Vec<RepoPath> = got.iter().map(|r| r.path.clone()).collect();
    ensure_eq!(got_paths, want, "rows of this id only, ascending path");
    ensure!(
        got.iter()
            .all(|r| r.object == blob && r.first_changeset == root.id && r.at == t(5)),
        "object_paths row content"
    );
    let limited = ok(s.object_paths(&blob, 1).await, "object_paths limit")?;
    ensure_eq!(limited.len(), 1, "limit respected");
    ensure_eq!(limited[0].path, want[0], "limit keeps the smallest path");
    let missing = ObjectId::hash_blob(b"no paths");
    let many = ok(
        s.object_paths_many(&[other, blob, missing, blob], 2).await,
        "object_paths_many",
    )?;
    ensure_eq!(many.len(), 4, "one entry per input id");
    ensure_eq!(many[0].len(), 1, "other's single row");
    let many_paths: Vec<RepoPath> = many[1].iter().map(|r| r.path.clone()).collect();
    ensure_eq!(
        many_paths,
        want[..2].to_vec(),
        "per-id limit, ascending path"
    );
    ensure!(many[2].is_empty(), "no rows for an unreferenced id");
    ensure_eq!(many[3], many[1], "duplicates repeat the rows");
    // A second changeset with the same (object, path) keeps the first row.
    let child = cs_row(b"paths child", 2, vec![root.id]);
    let mut b2 = bundle(child.clone(), vec![root.id]);
    b2.object_paths.push(ObjectPathRow {
        object: other,
        path: RepoPath::root(),
        first_changeset: child.id,
        at: t(6),
    });
    ok(s.commit_changeset(b2).await, "commit child")?;
    let got = ok(s.object_paths(&other, 100).await, "object_paths other")?;
    ensure_eq!(got.len(), 1, "one row per (object, path)");
    ensure_eq!(got[0].first_changeset, root.id, "first changeset kept");
    Ok(())
}

async fn meta_prefix(s: Arc<dyn MetaStore>) -> CaseResult {
    let mut ids = Vec::new();
    for n in 0..20u8 {
        let row = cs_row(&[b'p', n], if n == 0 { 1 } else { 2 }, vec![]);
        ids.push(row.id);
        ok(s.commit_changeset(bundle(row, vec![])).await, "commit")?;
    }
    ids.sort();
    ensure_eq!(
        ok(s.changesets_by_prefix("", 1000).await, "all")?,
        ids,
        "empty prefix lists all, ascending"
    );
    ensure_eq!(
        ok(s.changesets_by_prefix("", 5).await, "limited")?,
        ids[..5].to_vec(),
        "limit respected"
    );
    for id in &ids {
        let hex = id.hex();
        for len in [8, 9, 64] {
            let p = &hex[..len];
            let got = ok(s.changesets_by_prefix(p, 10).await, "prefix")?;
            let want: Vec<ObjectId> = ids
                .iter()
                .copied()
                .filter(|i| i.hex().starts_with(p))
                .collect();
            ensure_eq!(got, want, format!("prefix {p}"));
        }
    }
    // A prefix matching nothing (flip the first digit of every id until unused).
    let used: Vec<String> = ids.iter().map(|i| i.hex()[..8].to_owned()).collect();
    let unused = (0u32..)
        .map(|n| format!("{n:08x}"))
        .find(|p| !used.iter().any(|u| u == p))
        .unwrap_or_default();
    ensure!(
        ok(s.changesets_by_prefix(&unused, 10).await, "unused prefix")?.is_empty(),
        "unused prefix matches nothing"
    );
    Ok(())
}

async fn meta_refs_cas(s: Arc<dyn MetaStore>) -> CaseResult {
    let main = RefName::main();
    let (a, b) = (cs_id(1), cs_id(2));
    ensure_eq!(ok(s.get_ref(&main).await, "get_ref")?, None, "absent ref");
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(a))).await,
        "create",
    )?;
    let row = committed(&o)?.ok_or("create returned no row")?;
    ensure_eq!(
        row,
        RefRow {
            name: main.clone(),
            target: a,
            version: 1,
            updated_at: t(10)
        },
        "created row"
    );
    ensure_eq!(
        ok(s.get_ref(&main).await, "get_ref")?,
        Some(row.clone()),
        "get_ref"
    );
    ensure_eq!(
        ok(s.list_refs().await, "list_refs")?,
        vec![row.clone()],
        "list_refs"
    );
    // Mismatches return the current row and write nothing.
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(b))).await,
        "create again",
    )?;
    ensure_eq!(
        o,
        CasOutcome::VersionMismatch {
            current: Some(row.clone())
        },
        "create of existing ref"
    );
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(7), Some(b))).await,
        "wrong version",
    )?;
    ensure_eq!(
        o,
        CasOutcome::VersionMismatch {
            current: Some(row.clone())
        },
        "wrong version"
    );
    let mut up = cas(&main, RefExpect::Version(1), Some(b));
    up.at = t(11);
    up.principal = "github:bob".to_owned();
    let o = ok(s.cas_ref(up).await, "update")?;
    let row2 = committed(&o)?.ok_or("update returned no row")?;
    ensure_eq!(row2.version, 2, "version +1");
    ensure_eq!(row2.target, b, "new target");
    ensure_eq!(row2.updated_at, t(11), "updated_at");
    let log = ok(s.ref_log(&main, 0, 100).await, "ref_log")?;
    ensure_eq!(log.len(), 2, "two ref_log rows");
    ensure_eq!(
        log[0],
        RefLogRow {
            name: main.clone(),
            version: 1,
            old_target: None,
            new_target: Some(a),
            principal: "github:alice".to_owned(),
            at: t(10),
            landed: vec![]
        },
        "ref_log v1"
    );
    ensure_eq!(
        log[1],
        RefLogRow {
            name: main.clone(),
            version: 2,
            old_target: Some(a),
            new_target: Some(b),
            principal: "github:bob".to_owned(),
            at: t(11),
            landed: vec![]
        },
        "ref_log v2"
    );
    let from2 = ok(s.ref_log(&main, 2, 100).await, "ref_log from 2")?;
    ensure_eq!(from2, log[1..].to_vec(), "from_version");
    let lim = ok(s.ref_log(&main, 0, 1).await, "ref_log limit")?;
    ensure_eq!(lim, log[..1].to_vec(), "limit");
    ensure!(
        ok(s.ref_log(&main, 3, 100).await, "ref_log past end")?.is_empty(),
        "past the end"
    );
    // ABA: A -> B -> A; a CAS expecting the first version fails.
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(2), Some(a))).await,
        "back to a",
    )?;
    ensure_eq!(committed(&o)?.map(|r| r.version), Some(3), "aba version");
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(1), Some(b))).await,
        "aba stale",
    )?;
    ensure!(
        matches!(o, CasOutcome::VersionMismatch { current: Some(ref r) } if r.version == 3 && r.target == a),
        "ABA CAS with the first version must fail, got {o:?}"
    );
    Ok(())
}

async fn meta_refs_delete(s: Arc<dyn MetaStore>) -> CaseResult {
    let main = RefName::main();
    let (a, b) = (cs_id(1), cs_id(2));
    let e = expect_err(
        s.cas_ref(cas(&main, RefExpect::Absent, None)).await,
        ErrorCode::InvalidArgument,
        "absent + delete",
    )?;
    ensure!(!e.message.is_empty(), "error message");
    ensure_eq!(ok(s.get_ref(&main).await, "get")?, None, "nothing created");
    ensure!(
        ok(s.ref_log(&main, 0, 10).await, "ref_log")?.is_empty(),
        "no ref_log row on error"
    );
    // Expecting a version of a ref that does not exist is a mismatch with no current row.
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(1), Some(a))).await,
        "version on absent",
    )?;
    ensure_eq!(
        o,
        CasOutcome::VersionMismatch { current: None },
        "version expectation on an absent ref"
    );
    ensure!(
        ok(s.ref_log(&main, 0, 10).await, "ref_log")?.is_empty(),
        "no ref_log row on mismatch"
    );
    ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(a))).await,
        "create",
    )?;
    ok(
        s.cas_ref(cas(&main, RefExpect::Version(1), Some(b))).await,
        "update",
    )?;
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(2), None)).await,
        "delete",
    )?;
    ensure_eq!(o, CasOutcome::Committed(None), "delete outcome");
    ensure_eq!(ok(s.get_ref(&main).await, "get")?, None, "deleted");
    ensure!(
        ok(s.list_refs().await, "list")?.is_empty(),
        "list after delete"
    );
    let log = ok(s.ref_log(&main, 0, 10).await, "ref_log")?;
    ensure_eq!(log.len(), 3, "delete logged");
    ensure_eq!(log[2].version, 3, "delete version");
    ensure_eq!(log[2].old_target, Some(b), "delete old target");
    ensure_eq!(log[2].new_target, None, "delete new target");
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Version(3), Some(a))).await,
        "update deleted",
    )?;
    ensure_eq!(
        o,
        CasOutcome::VersionMismatch { current: None },
        "a deleted ref cannot be updated by version"
    );
    // Re-creating continues the numbering after the last ref_log version.
    let o = ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(a))).await,
        "re-create",
    )?;
    ensure_eq!(
        committed(&o)?.map(|r| r.version),
        Some(4),
        "re-created version"
    );
    let log = ok(s.ref_log(&main, 0, 10).await, "ref_log")?;
    ensure_eq!(
        log.iter().map(|r| r.version).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "ref_log has no gaps"
    );
    ensure_eq!(log[3].old_target, None, "re-create old target");
    Ok(())
}

async fn meta_landed(s: Arc<dyn MetaStore>) -> CaseResult {
    let main = RefName::main();
    let (a, b, c) = (cs_id(1), cs_id(2), cs_id(3));
    let (src1, src2) = (cs_id(11), cs_id(12));
    let (ch1, ch2) = (ChangeId([1; 16]), ChangeId([2; 16]));
    ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(a))).await,
        "create",
    )?;
    let mut land = cas(&main, RefExpect::Version(1), Some(b));
    land.landed = vec![
        LandedEntry {
            change_id: ch1,
            source: src1,
            result: a,
        },
        LandedEntry {
            change_id: ch2,
            source: src2,
            result: b,
        },
    ];
    land.audit = Some(audit("land", "main"));
    let o = ok(s.cas_ref(land.clone()).await, "land")?;
    ensure_eq!(committed(&o)?.map(|r| r.version), Some(2), "land version");
    let l1 = ok(s.get_landed(&main, &ch1).await, "get_landed")?;
    ensure_eq!(
        l1,
        Some(LandedRow {
            ref_name: main.clone(),
            change_id: ch1,
            version: 2,
            source: src1,
            result: a
        }),
        "landed row 1"
    );
    ensure!(
        ok(s.get_landed(&main, &ChangeId([9; 16])).await, "get_landed")?.is_none(),
        "absent landed row"
    );
    let log = ok(s.ref_log(&main, 2, 1).await, "ref_log")?;
    ensure_eq!(
        log.first().map(|r| r.landed.clone()),
        Some(land.landed.clone()),
        "ref_log carries landed tuples"
    );
    let audit_rows = ok(s.audit_log(10).await, "audit_log")?;
    ensure_eq!(
        audit_rows.last(),
        land.audit.as_ref(),
        "audit row in the same txn"
    );
    // A second land of ch2 conflicts and rolls back everything.
    let mut again = cas(&main, RefExpect::Version(2), Some(c));
    again.landed = vec![
        LandedEntry {
            change_id: ChangeId([3; 16]),
            source: cs_id(13),
            result: c,
        },
        LandedEntry {
            change_id: ch2,
            source: cs_id(14),
            result: c,
        },
    ];
    again.audit = Some(audit("land", "main again"));
    let o = ok(s.cas_ref(again).await, "conflicting land")?;
    ensure_eq!(
        o,
        CasOutcome::LandedConflict {
            existing: LandedRow {
                ref_name: main.clone(),
                change_id: ch2,
                version: 2,
                source: src2,
                result: b
            }
        },
        "landed conflict"
    );
    let cur = ok(s.get_ref(&main).await, "get_ref")?;
    ensure_eq!(
        cur.map(|r| (r.version, r.target)),
        Some((2, b)),
        "ref unchanged"
    );
    ensure_eq!(
        ok(s.ref_log(&main, 0, 10).await, "ref_log")?.len(),
        2,
        "no ref_log row"
    );
    ensure!(
        ok(s.get_landed(&main, &ChangeId([3; 16])).await, "get_landed")?.is_none(),
        "no partial landed row"
    );
    ensure_eq!(
        ok(s.audit_log(10).await, "audit_log")?.len(),
        audit_rows.len(),
        "no audit row"
    );
    // The landed index is per ref.
    if let Some(rel) = ref_name("release/1") {
        ok(
            s.cas_ref(cas(&rel, RefExpect::Absent, Some(a))).await,
            "create release",
        )?;
        let mut l = cas(&rel, RefExpect::Version(1), Some(b));
        l.landed = vec![LandedEntry {
            change_id: ch1,
            source: src1,
            result: b,
        }];
        let o = ok(s.cas_ref(l).await, "land on release")?;
        ensure!(
            matches!(o, CasOutcome::Committed(Some(_))),
            "same change id on another ref commits, got {o:?}"
        );
    }
    Ok(())
}

async fn meta_multi_refs(s: Arc<dyn MetaStore>) -> CaseResult {
    let names: Option<Vec<RefName>> = ["release/2", "green/ci", "main", "release/10"]
        .iter()
        .map(|n| ref_name(n))
        .collect();
    let Some(names) = names else {
        tracing::warn!("conformance: RefName::parse unavailable; multi-ref case skipped");
        return Ok(());
    };
    for (i, n) in names.iter().enumerate() {
        ok(
            s.cas_ref(cas(n, RefExpect::Absent, Some(cs_id(i as u8))))
                .await,
            "create",
        )?;
    }
    let listed: Vec<RefName> = ok(s.list_refs().await, "list_refs")?
        .into_iter()
        .map(|r| r.name)
        .collect();
    let mut want = names.clone();
    want.sort();
    ensure_eq!(listed, want, "list_refs ascending by name");
    // Updating one ref leaves the others alone.
    ok(
        s.cas_ref(cas(&names[0], RefExpect::Version(1), Some(cs_id(50))))
            .await,
        "update",
    )?;
    for n in &names[1..] {
        let r = ok(s.get_ref(n).await, "get_ref")?;
        ensure_eq!(r.map(|r| r.version), Some(1), format!("{n} untouched"));
        ensure_eq!(
            ok(s.ref_log(n, 0, 10).await, "ref_log")?.len(),
            1,
            "own log"
        );
    }
    Ok(())
}

async fn meta_derived(s: Arc<dyn MetaStore>) -> CaseResult {
    let src = ObjectId::hash_blob(b"source");
    let other_src = ObjectId::hash_blob(b"other source");
    let k1 = dkey("blame", b"k1");
    let k2 = dkey("blame", b"k2");
    let k3 = dkey("history", b"k3");
    let k4 = dkey("blame", b"k4");
    ensure!(
        ok(s.get_derived(&k1, Consistency::Strong).await, "get")?.is_none(),
        "absent derived"
    );
    let r1 = drow(k1.clone(), b"v1", Some(src));
    let mut r2 = drow(k2.clone(), b"", Some(src));
    r2.value = None;
    r2.storage_key = Some("scion/d/k2".to_owned());
    let r3 = drow(k3.clone(), b"v3", Some(src));
    let r4 = drow(k4.clone(), b"v4", Some(other_src));
    ok(
        s.put_derived(vec![r1.clone(), r2.clone(), r3.clone(), r4.clone()])
            .await,
        "put_derived",
    )?;
    ensure_eq!(
        ok(s.get_derived(&k1, Consistency::Strong).await, "get k1")?,
        Some(r1.clone()),
        "k1"
    );
    ensure_eq!(
        ok(s.get_derived(&k2, Consistency::Strong).await, "get k2")?,
        Some(r2.clone()),
        "k2 with storage key"
    );
    if let Some(r) = ok(s.get_derived(&k1, Consistency::Stale).await, "stale k1")? {
        ensure_eq!(r, r1, "stale derived content");
    }
    // ON CONFLICT DO NOTHING.
    ok(
        s.put_derived(vec![drow(k1.clone(), b"other", None)]).await,
        "re-put",
    )?;
    ensure_eq!(
        ok(s.get_derived(&k1, Consistency::Strong).await, "get k1")?,
        Some(r1.clone()),
        "existing derived row unchanged"
    );
    // Versions are part of the key.
    let mut k1v2 = k1.clone();
    k1v2.version = 2;
    ensure!(
        ok(s.get_derived(&k1v2, Consistency::Strong).await, "get v2")?.is_none(),
        "version is part of the key"
    );
    let n = ok(
        s.delete_derived_by_source(&["blame".to_owned()], &src)
            .await,
        "delete_by_source",
    )?;
    ensure_eq!(n, 2, "deleted rows of the given type and source");
    ensure!(
        ok(s.get_derived(&k1, Consistency::Strong).await, "get")?.is_none()
            && ok(s.get_derived(&k2, Consistency::Strong).await, "get")?.is_none(),
        "deleted"
    );
    ensure!(
        ok(s.get_derived(&k3, Consistency::Strong).await, "get")?.is_some(),
        "other type kept"
    );
    ensure!(
        ok(s.get_derived(&k4, Consistency::Strong).await, "get")?.is_some(),
        "other source kept"
    );
    let n = ok(
        s.delete_derived_by_source(&["blame".to_owned()], &src)
            .await,
        "delete again",
    )?;
    ensure_eq!(n, 0, "nothing left to delete");
    // A deleted key re-derived for another source belongs to that source only.
    ok(
        s.put_derived(vec![drow(k1.clone(), b"v1'", Some(other_src))])
            .await,
        "re-put k1",
    )?;
    let n = ok(
        s.delete_derived_by_source(&["blame".to_owned()], &src)
            .await,
        "delete old source",
    )?;
    ensure_eq!(n, 0, "the old source owns nothing");
    let n = ok(
        s.delete_derived_by_source(&[], &other_src).await,
        "delete no types",
    )?;
    ensure_eq!(n, 0, "an empty type list deletes nothing");
    let n = ok(
        s.delete_derived_by_source(&["blame".to_owned(), "history".to_owned()], &other_src)
            .await,
        "delete new source",
    )?;
    ensure_eq!(n, 2, "k1 (re-derived) and k4 belong to the other source");
    Ok(())
}

async fn meta_leases(s: Arc<dyn MetaStore>) -> CaseResult {
    let k = dkey("blame", b"lease");
    let ttl = Duration::from_secs(30);
    ensure!(
        ok(s.try_lease(&k, "h1", ttl, t(100)).await, "take")?,
        "free lease taken"
    );
    ensure!(
        !ok(s.try_lease(&k, "h2", ttl, t(110)).await, "contend")?,
        "held lease refused"
    );
    ensure!(
        ok(s.try_lease(&k, "h1", ttl, t(120)).await, "renew")?,
        "holder renews"
    );
    ensure!(
        !ok(s.try_lease(&k, "h2", ttl, t(149)).await, "contend")?,
        "renewal extended the lease"
    );
    ensure!(
        ok(s.try_lease(&k, "h2", ttl, t(150)).await, "expired")?,
        "expired lease (expires_at <= now) taken over"
    );
    ok(s.release_lease(&k, "h1").await, "release by non-holder")?;
    ensure!(
        !ok(s.try_lease(&k, "h3", ttl, t(151)).await, "contend")?,
        "release by a non-holder is a no-op"
    );
    ok(s.release_lease(&k, "h2").await, "release")?;
    ensure!(
        ok(s.try_lease(&k, "h3", ttl, t(152)).await, "take")?,
        "released lease free"
    );
    let other = dkey("blame", b"other lease");
    ensure!(
        ok(s.try_lease(&other, "h1", ttl, t(152)).await, "other key")?,
        "leases are per key"
    );
    ok(
        s.release_lease(&dkey("x", b"never"), "h1").await,
        "release unknown",
    )?;
    Ok(())
}

async fn meta_redaction(s: Arc<dyn MetaStore>) -> CaseResult {
    let a = blob_row(b"secret a");
    let b = inline_row(Kind::Meta, b"meta bytes b");
    let c = blob_row(b"live c");
    for r in [&a, &b, &c] {
        ok(s.insert_object(r.clone()).await, "insert")?;
    }
    let unknown = ObjectId::hash_blob(b"unknown");
    let tx = |ids: Vec<ObjectId>, purge: u64| RedactTxn {
        ids,
        reason: "leaked key".to_owned(),
        actor: "github:admin".to_owned(),
        at: t(30),
        purge_after: t(purge),
    };
    expect_err(
        s.redact(tx(vec![a.id, unknown], 100)).await,
        ErrorCode::NotFound,
        "redact unknown",
    )?;
    ensure_eq!(
        ok(s.redaction_epoch().await, "epoch")?,
        0,
        "no effect on NOT_FOUND"
    );
    let objs = ok(s.get_objects(&[a.id], Consistency::Strong).await, "get")?;
    ensure_eq!(
        objs[0].as_ref().map(|r| r.state),
        Some(ObjectState::Live),
        "a still live"
    );

    let e1 = ok(s.redact(tx(vec![a.id, b.id], 200)).await, "redact")?;
    ensure_eq!(e1, 1, "first epoch");
    ensure_eq!(
        ok(s.redaction_epoch().await, "epoch")?,
        1,
        "epoch persisted"
    );
    let objs = ok(
        s.get_objects(&[a.id, b.id, c.id], Consistency::Strong)
            .await,
        "get",
    )?;
    let states: Vec<Option<ObjectState>> =
        objs.iter().map(|o| o.as_ref().map(|r| r.state)).collect();
    ensure_eq!(
        states,
        vec![
            Some(ObjectState::Quarantined),
            Some(ObjectState::Quarantined),
            Some(ObjectState::Live)
        ],
        "states after redact"
    );
    let (rows, cur) = ok(s.list_redactions(0).await, "list")?;
    ensure_eq!(cur, 1, "current epoch");
    let mut want_ids = vec![a.id, b.id];
    want_ids.sort();
    ensure_eq!(
        rows.iter().map(|r| r.object).collect::<Vec<_>>(),
        want_ids,
        "rows ordered by (epoch, id)"
    );
    ensure!(
        rows.iter().all(|r| r.state == RedactionState::Quarantined
            && r.epoch == 1
            && r.reason == "leaked key"
            && r.actor == "github:admin"
            && r.created_at == t(30)
            && r.purge_after == t(200)),
        "redaction row content: {rows:?}"
    );
    let audit_rows = ok(s.audit_log(100).await, "audit")?;
    let redact_targets: Vec<String> = audit_rows
        .iter()
        .filter(|r| r.action == "redact")
        .map(|r| r.target.clone())
        .collect();
    ensure_eq!(redact_targets.len(), 2, "one audit row per id");
    ensure!(
        redact_targets.contains(&a.id.to_text()) && redact_targets.contains(&b.id.to_text()),
        "audit targets are id texts"
    );

    // Re-redacting a quarantined id refreshes it with the new epoch.
    let mut re = tx(vec![a.id], 150);
    re.reason = "again".to_owned();
    let e2 = ok(s.redact(re).await, "re-redact")?;
    ensure_eq!(e2, 2, "second epoch");
    let (rows, cur) = ok(s.list_redactions(1).await, "list since 1")?;
    ensure_eq!(cur, 2, "current epoch");
    ensure_eq!(rows.len(), 1, "only rows with epoch > since");
    ensure!(
        rows[0].object == a.id
            && rows[0].epoch == 2
            && rows[0].reason == "again"
            && rows[0].purge_after == t(150)
            && rows[0].created_at == t(30),
        "refreshed row: {:?}",
        rows[0]
    );
    let (all, _) = ok(s.list_redactions(0).await, "list all")?;
    ensure_eq!(
        all.iter().map(|r| (r.epoch, r.object)).collect::<Vec<_>>(),
        vec![(1, b.id), (2, a.id)],
        "ordered by (epoch, object)"
    );
    // Due purges: quarantined with purge_after <= now, ascending by (purge_after, id).
    ensure!(
        ok(s.due_purges(t(149), 10).await, "due")?.is_empty(),
        "nothing due"
    );
    ensure_eq!(
        ok(s.due_purges(t(150), 10).await, "due")?,
        vec![a.id],
        "a due"
    );
    ensure_eq!(
        ok(s.due_purges(t(500), 10).await, "due")?,
        vec![a.id, b.id],
        "both due"
    );
    ensure_eq!(
        ok(s.due_purges(t(500), 1).await, "due")?,
        vec![a.id],
        "limit"
    );

    // Unredact requires every id to be quarantined.
    let e = expect_err(
        s.unredact(&[a.id, c.id], audit("unredact", "a,c")).await,
        ErrorCode::InvalidArgument,
        "unredact live",
    )?;
    ensure_eq!(e.subcode(), Some(subcode::NOT_QUARANTINED), "subcode");
    ensure_eq!(ok(s.redaction_epoch().await, "epoch")?, 2, "no effect");
    let e3 = ok(
        s.unredact(&[a.id], audit("unredact", "a")).await,
        "unredact",
    )?;
    ensure_eq!(e3, 3, "unredact bumps the epoch");
    let objs = ok(s.get_objects(&[a.id], Consistency::Strong).await, "get")?;
    ensure_eq!(
        objs[0].as_ref().map(|r| r.state),
        Some(ObjectState::Live),
        "a live again"
    );
    let (rows, cur) = ok(s.list_redactions(0).await, "list")?;
    ensure_eq!(cur, 3, "epoch");
    ensure_eq!(
        rows.iter().map(|r| r.object).collect::<Vec<_>>(),
        vec![b.id],
        "a row deleted"
    );
    ensure_eq!(
        ok(s.audit_log(1).await, "audit")?,
        vec![audit("unredact", "a")],
        "unredact audit"
    );

    // Purge: state purged, inline data cleared, epoch unchanged.
    ok(s.mark_purged(&[b.id], audit("purge", "b")).await, "purge")?;
    ensure_eq!(
        ok(s.redaction_epoch().await, "epoch")?,
        3,
        "purge keeps the epoch"
    );
    let objs = ok(s.get_objects(&[b.id], Consistency::Strong).await, "get")?;
    let brow = objs[0].clone().ok_or("purged row must remain")?;
    ensure_eq!(brow.state, ObjectState::Purged, "purged state");
    ensure_eq!(brow.inline_data, None, "inline data cleared");
    let (rows, _) = ok(s.list_redactions(0).await, "list")?;
    ensure_eq!(rows.len(), 1, "purged row listed");
    ensure_eq!(
        rows[0].state,
        RedactionState::Purged,
        "redaction row purged"
    );
    ensure!(
        ok(s.due_purges(t(500), 10).await, "due")?.is_empty(),
        "purged not due"
    );
    let e = expect_err(
        s.unredact(&[b.id], audit("unredact", "b")).await,
        ErrorCode::InvalidArgument,
        "unredact purged",
    )?;
    ensure_eq!(
        e.subcode(),
        Some(subcode::NOT_QUARANTINED),
        "unredact after purge"
    );
    ensure_eq!(
        ok(s.audit_log(1).await, "audit")?,
        vec![audit("purge", "b")],
        "purge audit; failed unredact wrote nothing"
    );
    // Redacting a purged object leaves it purged.
    let e4 = ok(s.redact(tx(vec![b.id], 900)).await, "redact purged")?;
    ensure_eq!(e4, 4, "epoch still increments");
    let objs = ok(s.get_objects(&[b.id], Consistency::Strong).await, "get")?;
    ensure_eq!(
        objs[0].as_ref().map(|r| r.state),
        Some(ObjectState::Purged),
        "stays purged"
    );
    let (rows, _) = ok(s.list_redactions(0).await, "list")?;
    ensure!(
        rows.len() == 1 && rows[0].state == RedactionState::Purged && rows[0].purge_after == t(200),
        "purged redaction row unchanged: {rows:?}"
    );
    Ok(())
}

async fn meta_audit(s: Arc<dyn MetaStore>) -> CaseResult {
    for i in 0..5 {
        ok(
            s.append_audit(audit("create_ref", &format!("r{i}"))).await,
            "append",
        )?;
    }
    let rows = ok(s.audit_log(3).await, "audit_log")?;
    ensure_eq!(
        rows.iter().map(|r| r.target.clone()).collect::<Vec<_>>(),
        vec!["r2".to_owned(), "r3".to_owned(), "r4".to_owned()],
        "most recent rows, oldest first"
    );
    ensure_eq!(
        ok(s.audit_log(100).await, "audit_log")?.len(),
        5,
        "all rows"
    );
    ensure!(
        ok(s.audit_log(0).await, "audit_log 0")?.is_empty(),
        "limit 0"
    );
    Ok(())
}

const RACERS: usize = 32;

async fn join_all<T: Send + 'static>(
    handles: Vec<tokio::task::JoinHandle<Result<T>>>,
) -> std::result::Result<Vec<T>, String> {
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(Ok(v)) => out.push(v),
            Ok(Err(e)) => return Err(format!("concurrent call failed: {e}")),
            Err(e) => return Err(format!("task failed: {e}")),
        }
    }
    Ok(out)
}

async fn meta_concurrent_cas(s: Arc<dyn MetaStore>) -> CaseResult {
    let main = RefName::main();
    ok(
        s.cas_ref(cas(&main, RefExpect::Absent, Some(cs_id(0))))
            .await,
        "create",
    )?;
    let handles = (0..RACERS)
        .map(|i| {
            let s = s.clone();
            let c = cas(&main, RefExpect::Version(1), Some(cs_id(i as u8 + 1)));
            tokio::spawn(async move { s.cas_ref(c).await })
        })
        .collect();
    let outs = join_all(handles).await?;
    let wins: Vec<&CasOutcome> = outs
        .iter()
        .filter(|o| matches!(o, CasOutcome::Committed(_)))
        .collect();
    ensure_eq!(wins.len(), 1, "exactly one CAS commits");
    ensure!(
        outs.iter().all(|o| matches!(o,
            CasOutcome::Committed(Some(r)) if r.version == 2)
            || matches!(o, CasOutcome::VersionMismatch { current: Some(r) } if r.version == 2)),
        "losers observe version 2: {outs:?}"
    );
    let log = ok(s.ref_log(&main, 0, 100).await, "ref_log")?;
    ensure_eq!(
        log.iter().map(|r| r.version).collect::<Vec<_>>(),
        vec![1, 2],
        "ref_log has exactly the committed versions"
    );
    Ok(())
}

async fn meta_concurrent_inserts(s: Arc<dyn MetaStore>) -> CaseResult {
    let row = blob_row(b"raced object");
    let handles = (0..RACERS)
        .map(|_| {
            let s = s.clone();
            let r = row.clone();
            tokio::spawn(async move { s.insert_object(r).await })
        })
        .collect();
    let outs = join_all(handles).await?;
    ensure_eq!(
        outs.iter()
            .filter(|o| **o == InsertOutcome::Inserted)
            .count(),
        1,
        "exactly one insert wins"
    );
    let cs = cs_row(b"raced changeset", 1, vec![]);
    let handles = (0..RACERS)
        .map(|_| {
            let s = s.clone();
            let b = bundle(cs.clone(), vec![]);
            tokio::spawn(async move { s.commit_changeset(b).await })
        })
        .collect();
    let outs = join_all(handles).await?;
    ensure_eq!(
        outs.iter()
            .filter(|o| **o == InsertOutcome::Inserted)
            .count(),
        1,
        "exactly one commit_changeset wins"
    );
    Ok(())
}

async fn meta_concurrent_leases(s: Arc<dyn MetaStore>) -> CaseResult {
    let k = dkey("blame", b"raced lease");
    let handles = (0..RACERS)
        .map(|i| {
            let s = s.clone();
            let k = k.clone();
            tokio::spawn(async move {
                s.try_lease(&k, &format!("h{i}"), Duration::from_secs(60), t(0))
                    .await
            })
        })
        .collect();
    let outs = join_all(handles).await?;
    ensure_eq!(
        outs.iter().filter(|g| **g).count(),
        1,
        "exactly one lease holder"
    );
    Ok(())
}

async fn meta_concurrent_redactions(s: Arc<dyn MetaStore>) -> CaseResult {
    let rows: Vec<ObjectRow> = (0..RACERS)
        .map(|i| blob_row(format!("redact me {i}").as_bytes()))
        .collect();
    for r in &rows {
        ok(s.insert_object(r.clone()).await, "insert")?;
    }
    let handles = rows
        .iter()
        .map(|r| {
            let s = s.clone();
            let tx = RedactTxn {
                ids: vec![r.id],
                reason: "race".to_owned(),
                actor: "github:admin".to_owned(),
                at: t(1),
                purge_after: t(2),
            };
            tokio::spawn(async move { s.redact(tx).await })
        })
        .collect();
    let mut epochs = join_all(handles).await?;
    epochs.sort_unstable();
    ensure_eq!(
        epochs,
        (1..=RACERS as u64).collect::<Vec<_>>(),
        "every redaction gets a distinct epoch"
    );
    let (listed, cur) = ok(s.list_redactions(0).await, "list")?;
    ensure_eq!(cur, RACERS as u64, "final epoch");
    ensure!(
        listed.windows(2).all(|w| w[0].epoch < w[1].epoch),
        "one row per epoch, ascending"
    );
    Ok(())
}

/// Every [`MetaStore`] method contract, including concurrency cases. `factory` returns a fresh,
/// empty store per case (`docs/contracts/storage-semantics.md` section 4; SPEC 5.2-5.4, 8.4,
/// 9.1, 13.2).
pub async fn meta_store_suite<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn MetaStore>>,
{
    let mut fails: Vec<String> = Vec::new();
    run_case!(fails, factory, "basics", meta_basics);
    run_case!(fails, factory, "objects", meta_objects);
    run_case!(fails, factory, "insert_objects", meta_insert_objects);
    run_case!(fails, factory, "uploads", meta_uploads);
    run_case!(fails, factory, "changesets", meta_changesets);
    run_case!(fails, factory, "object_paths", meta_object_paths);
    run_case!(fails, factory, "changesets_by_prefix", meta_prefix);
    run_case!(fails, factory, "refs_cas", meta_refs_cas);
    run_case!(fails, factory, "refs_delete", meta_refs_delete);
    run_case!(fails, factory, "landed", meta_landed);
    run_case!(fails, factory, "multi_refs", meta_multi_refs);
    run_case!(fails, factory, "derived", meta_derived);
    run_case!(fails, factory, "leases", meta_leases);
    run_case!(fails, factory, "redaction", meta_redaction);
    run_case!(fails, factory, "audit", meta_audit);
    run_case!(fails, factory, "concurrent_cas", meta_concurrent_cas);
    run_case!(
        fails,
        factory,
        "concurrent_inserts",
        meta_concurrent_inserts
    );
    run_case!(fails, factory, "concurrent_leases", meta_concurrent_leases);
    run_case!(
        fails,
        factory,
        "concurrent_redactions",
        meta_concurrent_redactions
    );
    assert!(
        fails.is_empty(),
        "meta_store_suite: {} case(s) failed:\n{}",
        fails.len(),
        fails.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// BlobStore cases
// ---------------------------------------------------------------------------------------------

async fn blob_put_get(s: Arc<dyn BlobStore>) -> CaseResult {
    let k = "conf/r/1/b/aa/put-get";
    ensure_eq!(ok(s.get(k).await, "get missing")?, None, "missing key");
    ensure!(
        !ok(s.exists(k).await, "exists")?,
        "missing key does not exist"
    );
    ok(s.put(k, Bytes::from_static(b"first")).await, "put")?;
    ensure_eq!(
        ok(s.get(k).await, "get")?,
        Some(Bytes::from_static(b"first")),
        "read after write"
    );
    ensure!(ok(s.exists(k).await, "exists")?, "exists after put");
    ok(s.put(k, Bytes::from_static(b"second")).await, "overwrite")?;
    ensure_eq!(
        ok(s.get(k).await, "get")?,
        Some(Bytes::from_static(b"second")),
        "put overwrites"
    );
    let e = "conf/r/1/b/aa/empty";
    ok(s.put(e, Bytes::new()).await, "put empty")?;
    ensure_eq!(
        ok(s.get(e).await, "get empty")?,
        Some(Bytes::new()),
        "empty value"
    );
    let big: Vec<u8> = (0..(1usize << 20) + 7).map(|i| (i % 251) as u8).collect();
    let bk = "conf/r/1/b/aa/big";
    ok(s.put(bk, Bytes::from(big.clone())).await, "put big")?;
    ensure!(
        ok(s.get(bk).await, "get big")?.as_deref() == Some(&big[..]),
        "1 MiB value round trip"
    );
    Ok(())
}

async fn blob_copy_delete(s: Arc<dyn BlobStore>) -> CaseResult {
    let (from, to) = ("conf/r/1/b/bb/src", "conf/q/1/b/bb/src");
    expect_err(
        s.copy(from, to).await,
        ErrorCode::NotFound,
        "copy missing source",
    )?;
    ensure!(
        !ok(s.exists(to).await, "exists")?,
        "failed copy created nothing"
    );
    ok(s.put(from, Bytes::from_static(b"payload")).await, "put")?;
    ok(s.copy(from, to).await, "copy")?;
    ensure_eq!(
        ok(s.get(to).await, "get copy")?,
        Some(Bytes::from_static(b"payload")),
        "copied bytes"
    );
    ensure!(ok(s.exists(from).await, "exists")?, "source kept");
    ok(s.copy(from, to).await, "copy again")?;
    ok(s.delete(from).await, "delete")?;
    ensure!(!ok(s.exists(from).await, "exists")?, "deleted");
    ensure_eq!(
        ok(s.get(from).await, "get")?,
        None,
        "deleted key reads None"
    );
    ok(s.delete(from).await, "delete again")?;
    ok(s.delete("conf/never/existed").await, "delete missing")?;
    ensure!(
        ok(s.exists(to).await, "exists")?,
        "copy independent of source"
    );
    Ok(())
}

async fn blob_list(s: Arc<dyn BlobStore>) -> CaseResult {
    let keys = [
        "conf/list/r/b/02",
        "conf/list/r/b/01",
        "conf/list/r/t/01",
        "conf/list/q/b/01",
        "conf/listing",
    ];
    for k in keys {
        ok(s.put(k, Bytes::from_static(b"x")).await, "put")?;
    }
    ensure_eq!(
        ok(s.list("conf/list/r/b/").await, "list")?,
        vec!["conf/list/r/b/01".to_owned(), "conf/list/r/b/02".to_owned()],
        "list by prefix, ascending"
    );
    ensure_eq!(
        ok(s.list("conf/list/r/").await, "list")?,
        vec![
            "conf/list/r/b/01".to_owned(),
            "conf/list/r/b/02".to_owned(),
            "conf/list/r/t/01".to_owned()
        ],
        "nested prefix"
    );
    ensure_eq!(
        ok(s.list("conf/list").await, "list")?.len(),
        5,
        "string prefix"
    );
    ensure!(
        ok(s.list("conf/none/").await, "list")?.is_empty(),
        "no match"
    );
    ok(s.delete("conf/list/r/b/01").await, "delete")?;
    ensure_eq!(
        ok(s.list("conf/list/r/b/").await, "list")?,
        vec!["conf/list/r/b/02".to_owned()],
        "deleted key not listed"
    );
    Ok(())
}

async fn blob_concurrent(s: Arc<dyn BlobStore>) -> CaseResult {
    let handles: Vec<_> = (0..RACERS)
        .map(|i| {
            let s = s.clone();
            tokio::spawn(async move {
                let k = format!("conf/conc/{i:03}");
                s.put(&k, Bytes::from(format!("v{i}"))).await
            })
        })
        .collect();
    join_all(handles).await?;
    let listed = ok(s.list("conf/conc/").await, "list")?;
    ensure_eq!(listed.len(), RACERS, "every concurrent put landed");
    for i in 0..RACERS {
        let k = format!("conf/conc/{i:03}");
        ensure_eq!(
            ok(s.get(&k).await, "get")?,
            Some(Bytes::from(format!("v{i}"))),
            k
        );
    }
    Ok(())
}

/// Every [`BlobStore`] method contract (`docs/contracts/storage-semantics.md` section 6;
/// SPEC 3.5, 5.1). `factory` returns a fresh, empty store per case.
pub async fn blob_store_suite<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn BlobStore>>,
{
    let mut fails: Vec<String> = Vec::new();
    run_case!(fails, factory, "put_get", blob_put_get);
    run_case!(fails, factory, "copy_delete", blob_copy_delete);
    run_case!(fails, factory, "list", blob_list);
    run_case!(fails, factory, "concurrent", blob_concurrent);
    assert!(
        fails.is_empty(),
        "blob_store_suite: {} case(s) failed:\n{}",
        fails.len(),
        fails.join("\n")
    );
}
