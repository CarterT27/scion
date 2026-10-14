//! SPEC 16.2 recovery invariants (PLAN 2.22, 3.4 item 3; P30).
//!
//! [`check_16_2`] is the oracle every failure-injection suite ends with:
//!
//! 1. **No visible changeset violates SPEC 4.5**: `scion_store::fsck` over the shared storage
//!    with every registered eager type required and `verify_bytes` on, so every live object's
//!    stored bytes (reachable from a changeset or listed in object storage) must hash to its
//!    id.
//! 2. **No ref moved except by a successful, authorized update**: every ref's `ref_log` must be
//!    a gap-free chain (`old_target` of each entry equals the previous entry's `new_target`,
//!    versions increasing), its last entry must equal the ref's current `(version, target)`,
//!    every target named in the log must be an existing changeset, and every `landed` entry a
//!    log row records must be the one stored in the `landed` index. Ref log rows are written
//!    only in the same transaction as a committed CAS, so this is ref-log provenance.
//! 3. **No tier serves bytes that do not match their identifier**: besides (1) for the
//!    server's storage, every entry of the daemon's cache (the bytes the daemon serves) is read
//!    back, decompressed and hashed.

use crate::cluster::TestCluster;
use scion_cache::Payload;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError};
use scion_store::{Consistency, FsckOptions, RefLogRow, fsck};
use std::path::Path;

/// Largest ref log read per ref.
const REF_LOG_LIMIT: usize = 1_000_000;

fn violation(msg: String) -> ScionError {
    ScionError::new(ErrorCode::Integrity, format!("SPEC 16.2 violation: {msg}"))
}

/// fsck, ref-log provenance and digest checks on everything served during the run
/// (SPEC 16.2; see the module docs).
pub async fn check_16_2(c: &TestCluster) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    check_fsck(c, &mut problems).await?;
    check_ref_logs(c, &mut problems).await?;
    check_daemon_cache(c, &mut problems)?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(violation(problems.join("; ")))
    }
}

/// (1) SPEC 4.5 over the shared storage.
async fn check_fsck(c: &TestCluster, problems: &mut Vec<String>) -> Result<()> {
    let ctx = c.servers[0].ctx();
    let opts = FsckOptions {
        required_eager: ctx.derived.registry().eager_pairs(),
        verify_bytes: true,
    };
    for v in fsck(&ctx.objects, &opts).await? {
        problems.push(format!("fsck: {v:?}"));
    }
    Ok(())
}

/// (2) Ref-log provenance of every ref.
async fn check_ref_logs(c: &TestCluster, problems: &mut Vec<String>) -> Result<()> {
    let meta = &c.storage.meta;
    for r in meta.list_refs().await? {
        let name = r.name.as_str().to_owned();
        let log = meta.ref_log(&r.name, 0, REF_LOG_LIMIT).await?;
        check_chain(&name, &log, problems);
        match log.last() {
            Some(last) if last.version == r.version && last.new_target == Some(r.target) => {}
            Some(last) => problems.push(format!(
                "ref {name} is at version {} -> {} but its last log entry is version {} -> {:?}",
                r.version, r.target, last.version, last.new_target
            )),
            None => problems.push(format!("ref {name} has no ref log")),
        }
        let targets: Vec<ObjectId> = log.iter().filter_map(|e| e.new_target).collect();
        let rows = meta.get_changesets(&targets, Consistency::Strong).await?;
        for (t, row) in targets.iter().zip(rows) {
            if row.is_none() {
                problems.push(format!("ref {name} log names a missing changeset {t}"));
            }
        }
        for e in &log {
            for l in &e.landed {
                match meta.get_landed(&r.name, &l.change_id).await? {
                    Some(row)
                        if row.source == l.source
                            && row.result == l.result
                            && row.version == e.version => {}
                    other => problems.push(format!(
                        "ref {name} version {} records landed {} but the landed index has {other:?}",
                        e.version,
                        l.change_id.to_hex()
                    )),
                }
            }
        }
    }
    Ok(())
}

/// The ref log of one ref must be a gap-free chain of committed updates: versions start at 1
/// and increase by exactly 1 (SPEC 4.3.6; re-creating a deleted ref continues the numbering,
/// docs/contracts/storage-semantics.md), the first entry creates the ref, and each entry starts
/// from the target the previous one ended at.
fn check_chain(name: &str, log: &[RefLogRow], problems: &mut Vec<String>) {
    let mut prev: Option<&RefLogRow> = None;
    for e in log {
        match prev {
            Some(p) => {
                if e.version != p.version.saturating_add(1) {
                    problems.push(format!(
                        "ref {name} log has a gap or reordering (version {} after {})",
                        e.version, p.version
                    ));
                }
                if e.old_target != p.new_target {
                    problems.push(format!(
                        "ref {name} version {} starts from {:?} but version {} ended at {:?}",
                        e.version, e.old_target, p.version, p.new_target
                    ));
                }
            }
            None => {
                if e.version != 1 {
                    problems.push(format!(
                        "ref {name} log starts at version {} instead of 1",
                        e.version
                    ));
                }
                if e.old_target.is_some() {
                    problems.push(format!(
                        "ref {name} version {} (first) has an old target",
                        e.version
                    ));
                }
            }
        }
        prev = Some(e);
    }
}

/// (3) Every entry of the daemon's cache must hash to its id.
fn check_daemon_cache(c: &TestCluster, problems: &mut Vec<String>) -> Result<()> {
    let Some(d) = &c.daemon else {
        return Ok(());
    };
    let root = d.cfg.cache_dir.join("objects");
    for id in cached_ids(&root) {
        match d.handle.cache().get_payload(&id)? {
            None => {} // Evicted or discarded as corrupt: never served.
            Some(p) => {
                let content = match p {
                    Payload::Raw(b) => b.to_vec(),
                    Payload::Zstd(b) => match zstd::decode_all(&b[..]) {
                        Ok(v) => v,
                        Err(e) => {
                            problems.push(format!("daemon cache entry {id} is not zstd: {e}"));
                            continue;
                        }
                    },
                };
                let actual = match id.kind() {
                    Kind::Blob => ObjectId::hash_blob(&content),
                    k => ObjectId::hash_encoded(k, &content),
                };
                if actual != id {
                    problems.push(format!("daemon cache entry {id} hashes to {actual}"));
                }
            }
        }
    }
    Ok(())
}

/// Ids of the files under `<cache_dir>/objects/<letter>/<hex[0:2]>/<hex>` (SPEC 10.2);
/// temporary and foreign files are skipped.
fn cached_ids(root: &Path) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let Ok(kinds) = std::fs::read_dir(root) else {
        return out;
    };
    for k in kinds.flatten() {
        let letter = k.file_name().to_string_lossy().into_owned();
        let Ok(fans) = std::fs::read_dir(k.path()) else {
            continue;
        };
        for f in fans.flatten() {
            let Ok(files) = std::fs::read_dir(f.path()) else {
                continue;
            };
            for e in files.flatten() {
                let hex = e.file_name().to_string_lossy().into_owned();
                // `x` covers derived kind 0x10 (`x1<hex>`); other kinds print their byte.
                if let Ok(id) = ObjectId::parse_text(&format!("{letter}1{hex}")) {
                    out.push(id);
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_object::RefName;
    use std::time::SystemTime;

    fn row(version: u64, old: Option<u8>, new: Option<u8>) -> RefLogRow {
        let id = |b: u8| ObjectId::hash_blob(&[b]);
        RefLogRow {
            name: RefName::parse("main").unwrap(),
            version,
            old_target: old.map(id),
            new_target: new.map(id),
            principal: "github:admin".into(),
            at: SystemTime::UNIX_EPOCH,
            landed: Vec::new(),
        }
    }

    fn problems(log: &[RefLogRow]) -> Vec<String> {
        let mut p = Vec::new();
        check_chain("main", log, &mut p);
        p
    }

    #[test]
    fn a_gap_free_chain_passes() {
        // Create, update, delete, re-create (numbering continues).
        let log = [
            row(1, None, Some(1)),
            row(2, Some(1), Some(2)),
            row(3, Some(2), None),
            row(4, None, Some(3)),
        ];
        assert_eq!(problems(&log), Vec::<String>::new());
    }

    #[test]
    fn gaps_wrong_start_and_broken_links_are_violations() {
        assert_eq!(
            problems(&[row(1, None, Some(1)), row(3, Some(1), Some(2))]).len(),
            1
        );
        assert_eq!(problems(&[row(2, None, Some(1))]).len(), 1);
        assert_eq!(problems(&[row(1, Some(9), Some(1))]).len(), 1);
        assert_eq!(
            problems(&[row(1, None, Some(1)), row(2, Some(7), Some(2))]).len(),
            1
        );
        assert_eq!(
            problems(&[row(1, None, Some(1)), row(1, Some(1), Some(2))]).len(),
            1
        );
    }

    #[test]
    fn cached_ids_parse_the_spec_layout() {
        let dir = tempfile::tempdir().unwrap();
        let id = ObjectId::hash_blob(b"layout");
        let hex = id.hex();
        let fan = dir.path().join("b").join(&hex[..2]);
        std::fs::create_dir_all(&fan).unwrap();
        std::fs::write(fan.join(&hex), b"x").unwrap();
        std::fs::write(fan.join("tmp-123"), b"x").unwrap();
        assert_eq!(cached_ids(dir.path()), vec![id]);
        assert!(cached_ids(&dir.path().join("missing")).is_empty());
    }
}
