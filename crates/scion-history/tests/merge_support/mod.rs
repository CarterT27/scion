//! Shared helpers for the P19 rebase/merge integration tests (SPEC 8.2, 8.3, 13.6, 19.4,
//! 19.13, 19.18).
#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{
    ChangeId, ConflictKind, ConflictReport, EntryInfo, ErrorCode, ErrorPayload, Kind, ManualClock,
    Mode, ObjectId, Principal, ScionError,
};
use scion_history::{AllowAll, CreateRequest, History, HistoryConfig, MetaFields, NoEager};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RepoPath, Timestamp};
use scion_refmodel::{MDelta, MFile, MTree};
use scion_store::{
    AuditRow, MemHandles, MemOptions, ObjectRepo, ObjectRepoConfig, RedactTxn, Storage,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// A history over in-memory storage plus the bootstrap root.
pub struct Env {
    pub h: History,
    pub storage: Storage,
    pub mem: MemHandles,
    pub clock: Arc<ManualClock>,
    pub root: ObjectId,
}

pub fn start_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// A second engine over `storage` (fresh ObjectRepo caches) with its own clock.
pub fn history_over(storage: Storage, cfg: HistoryConfig, clock: Arc<ManualClock>) -> History {
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    History::new(objects, cfg, Arc::new(NoEager), clock)
}

pub async fn env_with(cfg: HistoryConfig) -> Env {
    let (storage, mem) = Storage::in_memory(MemOptions::default());
    let clock = ManualClock::new(start_time());
    let h = history_over(storage.clone(), cfg, clock.clone());
    let root = h.bootstrap().await.expect("bootstrap");
    Env {
        h,
        storage,
        mem,
        clock,
        root,
    }
}

pub async fn env() -> Env {
    env_with(HistoryConfig::default()).await
}

/// A config whose `merge_max_text_bytes` is `max_text`.
pub fn cfg_max_text(max_text: u64) -> HistoryConfig {
    let mut cfg = HistoryConfig::default();
    cfg.limits.merge_max_text_bytes = max_text;
    cfg
}

pub fn p(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).expect("valid test path")
}

pub fn alice() -> Principal {
    Principal::GitHub {
        login: "alice".to_owned(),
        id: Some(7),
    }
}

pub fn bob() -> Principal {
    Principal::GitHub {
        login: "bob".to_owned(),
        id: Some(8),
    }
}

pub fn fields(message: &str) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

/// One change of a test commit: `Some((mode, content))` puts, `None` deletes.
pub type Change<'a> = (&'a str, Option<(Mode, &'a [u8])>);

pub fn file(c: &str) -> Option<(Mode, &[u8])> {
    Some((Mode::File, c.as_bytes()))
}

pub fn exec(c: &str) -> Option<(Mode, &[u8])> {
    Some((Mode::Executable, c.as_bytes()))
}

pub fn link(c: &str) -> Option<(Mode, &[u8])> {
    Some((Mode::Symlink, c.as_bytes()))
}

pub fn raw(mode: Mode, c: &[u8]) -> Option<(Mode, &[u8])> {
    Some((mode, c))
}

pub const DEL: Option<(Mode, &[u8])> = None;

/// Upload `content` and return its entry.
pub async fn upload(h: &History, mode: Mode, content: &[u8]) -> EntryInfo {
    let (id, _) = h
        .objects()
        .put_blob(Bytes::from(content.to_vec()))
        .await
        .expect("upload");
    EntryInfo {
        mode,
        id,
        size: content.len() as u64,
    }
}

/// A deterministic change id for a commit (a function of its parents and message).
pub fn change_id_for(parents: &[ObjectId], message: &str) -> ChangeId {
    let mut hasher = blake3::Hasher::new();
    for parent in parents {
        hasher.update(parent.as_bytes());
    }
    hasher.update(message.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    ChangeId(out)
}

/// Create a changeset on `parents` (delta relative to `parents[0]`) as alice.
pub async fn commit_delta(
    h: &History,
    parents: Vec<ObjectId>,
    mut delta: Vec<DeltaEntry>,
    message: &str,
) -> ObjectId {
    delta.sort_by(|a, b| a.path.cmp(&b.path));
    let import_mode = parents.is_empty();
    let req = CreateRequest {
        change_id: change_id_for(&parents, message),
        parents,
        delta,
        meta: fields(message),
        expected_root_tree: None,
        import_mode,
    };
    h.create_changeset(req, &alice(), &AllowAll)
        .await
        .expect("create_changeset")
        .changeset_id
}

/// Upload the contents and create a changeset on `parent` with `changes`.
pub async fn commit(h: &History, parent: ObjectId, changes: &[Change<'_>]) -> ObjectId {
    let mut delta = Vec::new();
    let mut message = String::from("commit");
    for (path, change) in changes {
        message.push(' ');
        message.push_str(path);
        let op = match change {
            Some((mode, content)) => {
                let e = upload(h, *mode, content).await;
                message.push_str(&e.id.hex()[..8]);
                DeltaOp::Put {
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                }
            }
            None => DeltaOp::Delete,
        };
        delta.push(DeltaEntry { path: p(path), op });
    }
    commit_delta(h, vec![parent], delta, &message).await
}

/// The SPEC 7.2 MERGE_CONFLICT payload of `err`.
pub fn report(err: &ScionError) -> &ConflictReport {
    assert_eq!(err.code, ErrorCode::MergeConflict, "unexpected error {err}");
    match &err.payload {
        ErrorPayload::MergeConflict(r) => r,
        other => panic!("MERGE_CONFLICT without a report: {other:?}"),
    }
}

/// `(path, kind)` of every conflict.
pub fn kinds(r: &ConflictReport) -> Vec<(String, ConflictKind)> {
    r.conflicts
        .iter()
        .map(|c| (String::from_utf8_lossy(&c.path).into_owned(), c.kind))
        .collect()
}

pub fn kind_str(k: ConflictKind) -> &'static str {
    match k {
        ConflictKind::AddAdd => "add_add",
        ConflictKind::ModifyDelete => "modify_delete",
        ConflictKind::Symlink => "symlink",
        ConflictKind::Binary => "binary",
        ConflictKind::Redacted => "redacted",
        ConflictKind::FileDirectory => "file_directory",
        ConflictKind::Content => "content",
    }
}

/// Root tree of a changeset.
pub async fn root_of(h: &History, c: &ObjectId) -> ObjectId {
    h.changeset(c).await.expect("changeset").changeset.root_tree
}

/// The file entry at `path` in changeset `c`.
pub async fn entry_at(h: &History, c: &ObjectId, path: &str) -> Option<EntryInfo> {
    let root = root_of(h, c).await;
    h.lookup(&root, &p(path))
        .await
        .expect("lookup")
        .filter(|e| e.mode != Mode::Tree)
        .map(|e| EntryInfo {
            mode: e.mode,
            id: e.id,
            size: e.size,
        })
}

/// The content of the file at `path` in changeset `c`.
pub async fn content_at(h: &History, c: &ObjectId, path: &str) -> Option<Vec<u8>> {
    match entry_at(h, c, path).await {
        Some(e) => Some(h.objects().get(&e.id).await.expect("blob").to_vec()),
        None => None,
    }
}

/// The structural (refmodel) snapshot of a stored root tree.
pub async fn snapshot(h: &History, root: &ObjectId) -> MTree {
    let mut out = MTree::new();
    let mut stack = vec![(Vec::<u8>::new(), *root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = h.tree(&id).await.expect("tree");
        for e in &tree.entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(e.name.as_bytes());
            if e.mode == Mode::Tree {
                stack.push((path, e.id));
            } else {
                let content = h.objects().get(&e.id).await.expect("blob").to_vec();
                out.insert(
                    path,
                    MFile {
                        mode: e.mode as u8,
                        content,
                    },
                );
            }
        }
    }
    out
}

pub fn mode_of(m: u8) -> Mode {
    Mode::from_u8(m).expect("file mode")
}

/// The delta (uploading every Put) that turns structural tree `from` into `to`.
pub async fn delta_between(h: &History, from: &MTree, to: &MTree) -> Vec<DeltaEntry> {
    let mut out = Vec::new();
    for d in scion_refmodel::file_diff(from, to) {
        match d {
            MDelta::Put(path, f) => {
                let e = upload(h, mode_of(f.mode), &f.content).await;
                out.push(DeltaEntry {
                    path: RepoPath::parse(&path).expect("path"),
                    op: DeltaOp::Put {
                        mode: e.mode,
                        id: e.id,
                        size: e.size,
                    },
                });
            }
            MDelta::Delete(path) => out.push(DeltaEntry {
                path: RepoPath::parse(&path).expect("path"),
                op: DeltaOp::Delete,
            }),
        }
    }
    out
}

/// Commit a structural tree on `parents` (parentless commits use import mode).
pub async fn commit_tree(
    h: &History,
    parents: Vec<ObjectId>,
    parent_tree: &MTree,
    tree: &MTree,
    message: &str,
) -> ObjectId {
    let delta = delta_between(h, parent_tree, tree).await;
    commit_delta(h, parents, delta, message).await
}

/// Quarantine `ids` (SPEC 13.2) directly in the metadata store.
pub async fn redact(env: &Env, ids: &[ObjectId]) {
    env.h
        .meta()
        .redact(RedactTxn {
            ids: ids.to_vec(),
            reason: "test".to_owned(),
            actor: "service:test".to_owned(),
            at: start_time(),
            purge_after: start_time() + Duration::from_secs(86_400),
        })
        .await
        .expect("redact");
}

/// Mark `ids` purged (SPEC 13.3) directly in the metadata store.
pub async fn purge(env: &Env, ids: &[ObjectId]) {
    env.h
        .meta()
        .mark_purged(
            ids,
            AuditRow {
                at: start_time(),
                actor: "service:test".to_owned(),
                action: "purge".to_owned(),
                target: String::new(),
                reason: "test".to_owned(),
                detail: String::new(),
            },
        )
        .await
        .expect("purge");
}

/// SPEC 19.4: no stored object contains a conflict marker (the engine never writes markers;
/// test inputs never contain them).
pub async fn assert_no_markers(env: &Env) {
    let mut scanned = 0usize;
    for row in env.mem.meta.all_objects() {
        if row.kind == Kind::Changeset {
            continue;
        }
        let bytes = match env.h.objects().get(&row.id).await {
            Ok(b) => b,
            Err(e) if e.code == ErrorCode::Redacted => continue,
            Err(e) => panic!("reading {}: {e}", row.id),
        };
        scanned += 1;
        for marker in [&b"<<<<<<<"[..], b">>>>>>>", b"|||||||"] {
            assert!(
                !bytes.windows(marker.len()).any(|w| w == marker),
                "object {} contains a conflict marker",
                row.id
            );
        }
    }
    for row in env.mem.meta.all_changesets() {
        assert!(!row.encoded.windows(7).any(|w| w == b"<<<<<<<"));
    }
    assert!(scanned > 0);
}
