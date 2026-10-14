//! Shared helpers for the P12 integration tests (SPEC 19.2, 19.3, 19.13, 19.18).
#![allow(dead_code)]

use bytes::Bytes;
use scion_core::{ChangeId, ManualClock, Mode, ObjectId, Principal, empty_tree_id};
use scion_history::{
    CreateRequest, Created, History, HistoryConfig, MetaFields, NoEager, WriteAuthorizer,
};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RepoPath, Timestamp};
use scion_store::{FsckOptions, MemHandles, MemOptions, ObjectRepo, ObjectRepoConfig, Storage};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub struct Env {
    pub h: History,
    pub mem: MemHandles,
    pub clock: Arc<ManualClock>,
    /// The bootstrap root changeset.
    pub root: ObjectId,
}

pub fn history_with(cfg: HistoryConfig) -> (History, MemHandles, Arc<ManualClock>) {
    let (storage, mem) = Storage::in_memory(MemOptions::default());
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    let h = History::new(objects, cfg, Arc::new(NoEager), clock.clone());
    (h, mem, clock)
}

pub async fn env_with(cfg: HistoryConfig) -> Env {
    let (h, mem, clock) = history_with(cfg);
    let root = h.bootstrap().await.expect("bootstrap");
    Env {
        h,
        mem,
        clock,
        root,
    }
}

pub async fn env() -> Env {
    env_with(HistoryConfig::default()).await
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

/// Upload `content` and return a Put of it at `path`.
pub async fn put(h: &History, path: &str, content: &str) -> DeltaEntry {
    let (id, _) = h
        .objects()
        .put_blob(Bytes::from(content.as_bytes().to_vec()))
        .await
        .expect("upload");
    DeltaEntry {
        path: p(path),
        op: DeltaOp::Put {
            mode: Mode::File,
            id,
            size: content.len() as u64,
        },
    }
}

/// A Put of `content` without uploading it.
pub fn put_unstored(path: &str, content: &str) -> DeltaEntry {
    DeltaEntry {
        path: p(path),
        op: DeltaOp::Put {
            mode: Mode::File,
            id: ObjectId::hash_blob(content.as_bytes()),
            size: content.len() as u64,
        },
    }
}

pub fn del(path: &str) -> DeltaEntry {
    DeltaEntry {
        path: p(path),
        op: DeltaOp::Delete,
    }
}

pub fn fields(message: &str) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 60,
        },
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

pub fn req(parents: Vec<ObjectId>, delta: Vec<DeltaEntry>) -> CreateRequest {
    CreateRequest {
        parents,
        delta,
        meta: fields("test change"),
        change_id: ChangeId([0x42; 16]),
        expected_root_tree: None,
        import_mode: false,
    }
}

/// Create a changeset as alice with every permission.
pub async fn create(h: &History, parents: Vec<ObjectId>, delta: Vec<DeltaEntry>) -> Created {
    h.create_changeset(req(parents, delta), &alice(), &scion_history::AllowAll)
        .await
        .expect("create_changeset")
}

/// fsck checking the eager types this crate writes itself plus every byte.
pub fn fsck_opts() -> FsckOptions {
    FsckOptions {
        required_eager: vec![("commit_graph".to_owned(), 1), ("path_index".to_owned(), 1)],
        verify_bytes: true,
    }
}

pub async fn assert_fsck_clean(h: &History) {
    let v = scion_store::fsck(h.objects(), &fsck_opts())
        .await
        .expect("fsck");
    assert!(v.is_empty(), "fsck violations: {v:?}");
}

/// Denies writes below `denied` and optionally import mode.
pub struct Deny {
    pub denied: Option<RepoPath>,
    pub import: bool,
}

impl WriteAuthorizer for Deny {
    fn can_write(&self, path: &RepoPath) -> bool {
        match &self.denied {
            Some(d) => !(d == path || d.is_ancestor_of(path)),
            None => true,
        }
    }

    fn can_import(&self) -> bool {
        self.import
    }
}

pub fn empty_tree() -> ObjectId {
    empty_tree_id()
}
