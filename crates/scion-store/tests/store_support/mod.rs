//! Shared helpers for the scion-store integration tests.
#![allow(dead_code)]

use bytes::Bytes;
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, Meta, Name, RefName, RepoPath,
    Timestamp, Tree, TreeEntry,
};
use scion_store::{
    ChangesetBundle, ChangesetRow, MemHandles, MemOptions, ObjectRepo, ObjectRepoConfig,
    ObjectRow, ObjectState, Storage,
};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Whole-second test times.
pub fn t(n: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n)
}

/// An in-memory repo with default config except the given threshold.
pub fn repo_with(opts: MemOptions, cfg: ObjectRepoConfig) -> (ObjectRepo, MemHandles) {
    let (storage, handles) = Storage::in_memory(opts);
    (ObjectRepo::new(storage, cfg), handles)
}

pub fn repo() -> (ObjectRepo, MemHandles) {
    repo_with(MemOptions::default(), ObjectRepoConfig::default())
}

pub fn assert_code<T: std::fmt::Debug>(r: Result<T>, code: ErrorCode) -> scion_core::ScionError {
    match r {
        Ok(v) => panic!("expected {code:?}, got Ok({v:?})"),
        Err(e) => {
            assert_eq!(e.code, code, "unexpected error {e}");
            e
        }
    }
}

/// True once scion-object (P01) is implemented. Until then the tests that need its codec skip
/// with a `SKIP:` line (the stubs panic with `todo!`).
pub fn object_codec_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        std::panic::catch_unwind(|| {
            let blob = ObjectId::hash_blob(b"probe");
            let tree = Tree {
                entries: vec![TreeEntry {
                    name: Name::new(b"a").ok()?,
                    mode: scion_core::Mode::File,
                    id: blob,
                    size: 5,
                }],
                extensions: Extensions::new(),
            };
            let enc = tree.encode();
            let back = Tree::decode(&enc).ok()?;
            let meta = test_meta("probe");
            let menc = meta.encode();
            Meta::decode(&menc).ok()?;
            let cs = Changeset {
                parents: vec![],
                root_tree: tree.id(),
                meta: meta.id(),
                change_id: ChangeId([0; 16]),
                delta: vec![DeltaEntry {
                    path: RepoPath::parse(b"a").ok()?,
                    op: DeltaOp::Put {
                        mode: scion_core::Mode::File,
                        id: blob,
                        size: 5,
                    },
                }],
                extensions: Extensions::new(),
            };
            Changeset::decode(&cs.encode()).ok()?;
            RefName::parse("release/x").ok()?;
            Some(back == tree)
        })
        .ok()
        .flatten()
        .unwrap_or(false)
    })
}

/// Print a SKIP line and return true when the scion-object codec is unavailable.
pub fn skip_without_codec(test: &str) -> bool {
    if object_codec_ready() {
        return false;
    }
    println!("SKIP: {test}: scion-object (P01) codec not implemented in this build");
    true
}

pub fn test_meta(message: &str) -> Meta {
    Meta {
        author: b"Test <test@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"service:test".to_vec(),
        message: message.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

/// The 3-byte canonical empty tree (format_version 1, no entries, no extensions).
pub const EMPTY_TREE: &[u8] = &[0x01, 0x00, 0x00];

/// An objects row holding inline canonical bytes.
pub fn inline_row(kind: Kind, canonical: &[u8]) -> ObjectRow {
    ObjectRow {
        id: ObjectId::hash_encoded(kind, canonical),
        kind,
        size: canonical.len() as u64,
        stored_size: canonical.len() as u64,
        inline_data: Some(Bytes::copy_from_slice(canonical)),
        state: ObjectState::Live,
        created_at: t(0),
    }
}

/// A changesets row with arbitrary (not necessarily decodable) encoded bytes.
pub fn opaque_cs_row(encoded: &[u8], generation: u64) -> ChangesetRow {
    ChangesetRow {
        id: ObjectId::hash_encoded(Kind::Changeset, encoded),
        encoded: Bytes::copy_from_slice(encoded),
        generation,
        skip: vec![],
        change_id: ChangeId([7; 16]),
        created_by: "service:test".to_owned(),
        created_at: t(0),
    }
}

pub fn bundle(row: ChangesetRow, parents: Vec<ObjectId>) -> ChangesetBundle {
    ChangesetBundle {
        row,
        parents,
        object_paths: vec![],
        derived: vec![],
    }
}
