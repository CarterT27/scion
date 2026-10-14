//! Write-layer journal files under `.scion/` (SPEC 11.3, 11.6, 11.7, 16.1 class 8; P28).
//!
//! `PendingOperation::Commit` records only the parent and the change id, so the full
//! CreateChangeset request is kept next to the state in `.scion/commit-request`: recovery
//! re-issues exactly the same (deterministic, idempotent) request to learn whether the
//! changeset was created. A rebase paused on conflicts keeps the clean entries it could not
//! materialize (outside the sparse scope) in `.scion/rebase-clean`, so continuing the rebase
//! still carries them. Both files use the scion-object varint encoding with a version byte.

use crate::fs_safe::{io_error, write_atomic};
use scion_core::{ChangeId, EntryInfo, ErrorCode, Mode, ObjectId, Result, ScionError};
use scion_object::enc::{Decoder, Encoder};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RepoPath, Timestamp};
use scion_proto::convert::MetaFieldsParts;
use std::path::Path;

/// File holding the CreateChangeset request of a pending commit.
pub(crate) const COMMIT_REQUEST_FILE: &str = "commit-request";
/// File holding the out-of-scope clean entries of a rebase paused on conflicts.
pub(crate) const REBASE_CLEAN_FILE: &str = "rebase-clean";

const VERSION: u8 = 1;

/// A CreateChangeset request as the write layer issues it (SPEC 7.5, 11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitRequest {
    pub parents: Vec<ObjectId>,
    pub delta: Vec<DeltaEntry>,
    pub meta: MetaFieldsParts,
    pub change_id: ChangeId,
}

fn corrupt(what: &str) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("corrupt working-copy journal file: {what}"),
    )
}

fn put_entry(e: &mut Encoder, entry: Option<&EntryInfo>) {
    match entry {
        Some(x) => {
            e.u8(1);
            e.u8(x.mode as u8);
            e.id(&x.id);
            e.uvarint(x.size);
        }
        None => e.u8(0),
    }
}

fn get_entry(d: &mut Decoder<'_>) -> Result<Option<EntryInfo>> {
    if !d.option_tag()? {
        return Ok(None);
    }
    let mode = Mode::from_u8(d.u8()?)?;
    let id = d.id()?;
    let size = d.uvarint()?;
    Ok(Some(EntryInfo { mode, id, size }))
}

fn put_ts(e: &mut Encoder, t: &Timestamp) {
    e.svarint(t.seconds);
    e.svarint(t.tz_offset_minutes);
}

fn get_ts(d: &mut Decoder<'_>) -> Result<Timestamp> {
    Ok(Timestamp {
        seconds: d.svarint()?,
        tz_offset_minutes: d.svarint()?,
    })
}

/// The DeltaOp of an optional entry (`None` = Delete).
pub(crate) fn op_of(entry: Option<EntryInfo>) -> DeltaOp {
    match entry {
        Some(e) => DeltaOp::Put {
            mode: e.mode,
            id: e.id,
            size: e.size,
        },
        None => DeltaOp::Delete,
    }
}

/// The optional entry of a DeltaOp (`None` for Delete).
pub(crate) fn entry_of(op: &DeltaOp) -> Option<EntryInfo> {
    match op {
        DeltaOp::Put { mode, id, size } => Some(EntryInfo {
            mode: *mode,
            id: *id,
            size: *size,
        }),
        DeltaOp::Delete => None,
    }
}

impl CommitRequest {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u8(VERSION);
        e.uvarint(self.parents.len() as u64);
        for p in &self.parents {
            e.id(p);
        }
        e.uvarint(self.delta.len() as u64);
        for d in &self.delta {
            e.bytes(d.path.as_bytes());
            put_entry(&mut e, entry_of(&d.op).as_ref());
        }
        let m = &self.meta;
        e.bytes(&m.author);
        put_ts(&mut e, &m.author_time);
        match &m.committer {
            Some(c) => {
                e.u8(1);
                e.bytes(c);
            }
            None => e.u8(0),
        }
        match &m.committer_time {
            Some(t) => {
                e.u8(1);
                put_ts(&mut e, t);
            }
            None => e.u8(0),
        }
        e.bytes(&m.message);
        let ext: Vec<(u64, &[u8])> = m.extensions.iter().collect();
        e.uvarint(ext.len() as u64);
        for (tag, value) in ext {
            e.uvarint(tag);
            e.bytes(value);
        }
        e.raw(&self.change_id.0);
        e.finish()
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(b);
        if d.u8()? != VERSION {
            return Err(corrupt("unknown commit-request version"));
        }
        let n = d.list_len()?;
        let mut parents = Vec::with_capacity(n);
        for _ in 0..n {
            parents.push(d.id()?);
        }
        let n = d.list_len()?;
        let mut delta = Vec::with_capacity(n);
        for _ in 0..n {
            let path = RepoPath::parse(d.bytes()?)?;
            let op = op_of(get_entry(&mut d)?);
            delta.push(DeltaEntry { path, op });
        }
        let author = d.bytes()?.to_vec();
        let author_time = get_ts(&mut d)?;
        let committer = if d.option_tag()? {
            Some(d.bytes()?.to_vec())
        } else {
            None
        };
        let committer_time = if d.option_tag()? {
            Some(get_ts(&mut d)?)
        } else {
            None
        };
        let message = d.bytes()?.to_vec();
        let n = d.list_len()?;
        let mut extensions = Extensions::new();
        for _ in 0..n {
            let tag = d.uvarint()?;
            let value = d.bytes()?.to_vec();
            extensions.insert(tag, value)?;
        }
        let change_id = ChangeId(d.fixed::<16>()?);
        d.finish()?;
        Ok(CommitRequest {
            parents,
            delta,
            meta: MetaFieldsParts {
                author,
                author_time,
                committer,
                committer_time,
                message,
                extensions,
            },
            change_id,
        })
    }
}

/// Write `.scion/commit-request` (fsynced, atomic).
pub(crate) fn write_commit_request(scion_dir: &Path, req: &CommitRequest) -> Result<()> {
    write_atomic(scion_dir, COMMIT_REQUEST_FILE, &req.encode(), 0o644, true)
}

/// Read `.scion/commit-request`; `None` when absent or not decodable as a request.
pub(crate) fn read_commit_request(scion_dir: &Path) -> Result<Option<CommitRequest>> {
    match read_optional(&scion_dir.join(COMMIT_REQUEST_FILE))? {
        Some(b) => match CommitRequest::decode(&b) {
            Ok(r) => Ok(Some(r)),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring an undecodable commit journal");
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

/// Write `.scion/rebase-clean`: clean entries a paused rebase could not materialize.
pub(crate) fn write_rebase_clean(
    scion_dir: &Path,
    entries: &[(RepoPath, Option<EntryInfo>)],
) -> Result<()> {
    let mut e = Encoder::new();
    e.u8(VERSION);
    e.uvarint(entries.len() as u64);
    for (p, entry) in entries {
        e.bytes(p.as_bytes());
        put_entry(&mut e, entry.as_ref());
    }
    write_atomic(scion_dir, REBASE_CLEAN_FILE, &e.finish(), 0o644, true)
}

/// Read `.scion/rebase-clean` (empty when absent).
pub(crate) fn read_rebase_clean(scion_dir: &Path) -> Result<Vec<(RepoPath, Option<EntryInfo>)>> {
    let Some(b) = read_optional(&scion_dir.join(REBASE_CLEAN_FILE))? else {
        return Ok(Vec::new());
    };
    let mut d = Decoder::new(&b);
    if d.u8()? != VERSION {
        return Err(corrupt("unknown rebase-clean version"));
    }
    let n = d.list_len()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let p = RepoPath::parse(d.bytes()?)?;
        out.push((p, get_entry(&mut d)?));
    }
    d.finish()?;
    Ok(out)
}

/// Remove a journal file under `.scion/` (absent is fine).
pub(crate) fn remove_file(scion_dir: &Path, name: &str) -> Result<()> {
    match std::fs::remove_file(scion_dir.join(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("remove a journal file", e)),
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("read a journal file", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    #[test]
    fn commit_request_round_trip() {
        let mut ext = Extensions::new();
        ext.insert(2, b"git".to_vec()).unwrap();
        let req = CommitRequest {
            parents: vec![ObjectId::new(Kind::Changeset, [1; 32])],
            delta: vec![
                DeltaEntry {
                    path: RepoPath::parse(b"a/b").unwrap(),
                    op: DeltaOp::Put {
                        mode: Mode::Executable,
                        id: ObjectId::hash_blob(b"x"),
                        size: 1,
                    },
                },
                DeltaEntry {
                    path: RepoPath::parse(b"c").unwrap(),
                    op: DeltaOp::Delete,
                },
            ],
            meta: MetaFieldsParts {
                author: b"A <a@x>".to_vec(),
                author_time: Timestamp {
                    seconds: -5,
                    tz_offset_minutes: 330,
                },
                committer: Some(b"C".to_vec()),
                committer_time: Some(Timestamp {
                    seconds: 9,
                    tz_offset_minutes: -60,
                }),
                message: b"msg\n".to_vec(),
                extensions: ext,
            },
            change_id: ChangeId([7; 16]),
        };
        let b = req.encode();
        assert_eq!(CommitRequest::decode(&b).unwrap(), req);
        assert!(CommitRequest::decode(&b[..b.len() - 1]).is_err());

        let dir = tempfile::tempdir().unwrap();
        assert!(read_commit_request(dir.path()).unwrap().is_none());
        write_commit_request(dir.path(), &req).unwrap();
        assert_eq!(read_commit_request(dir.path()).unwrap(), Some(req));
        std::fs::write(dir.path().join(COMMIT_REQUEST_FILE), b"junk").unwrap();
        assert!(read_commit_request(dir.path()).unwrap().is_none());
        remove_file(dir.path(), COMMIT_REQUEST_FILE).unwrap();
        remove_file(dir.path(), COMMIT_REQUEST_FILE).unwrap();
    }

    #[test]
    fn rebase_clean_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_rebase_clean(dir.path()).unwrap().is_empty());
        let v = vec![
            (
                RepoPath::parse(b"z/y").unwrap(),
                Some(EntryInfo {
                    mode: Mode::Symlink,
                    id: ObjectId::hash_blob(b"t"),
                    size: 1,
                }),
            ),
            (RepoPath::parse(b"zz").unwrap(), None),
        ];
        write_rebase_clean(dir.path(), &v).unwrap();
        assert_eq!(read_rebase_clean(dir.path()).unwrap(), v);
    }
}
