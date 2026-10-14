//! `.scion/state`: canonical TOML, saved atomically (SPEC 4.3.10, 11.1; PLAN 6.15; P11).
//!
//! Canonical form: the state is converted to a TOML value whose tables have their keys
//! sorted bytewise at every level, then serialized; equal states always produce identical
//! bytes. Saves go to a temporary file in `.scion/` that is fsynced and renamed over `state`,
//! so a crash mid-save leaves the previous state intact.

use crate::fs_safe::{io_error, write_atomic};
use scion_core::{ChangeId, ErrorCode, ObjectId, Result, ScionError};
use scion_object::{RefName, RepoPath};
use std::path::Path;

/// File name of the state under `.scion/` (SPEC 11.1).
pub const STATE_FILE: &str = "state";

/// A journaled multi-step operation (SPEC 11.3, 11.6, 11.7).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PendingOperation {
    /// Initial checkout of `target`.
    Checkout {
        /// Target changeset.
        target: ObjectId,
    },
    /// Goto from `from` to `to`.
    Goto {
        /// Previous base.
        from: ObjectId,
        /// New base.
        to: ObjectId,
    },
    /// Sparse profile change.
    SparseChange {
        /// Old profile.
        #[serde(with = "crate::serde_util::paths")]
        old: Vec<RepoPath>,
        /// New profile.
        #[serde(with = "crate::serde_util::paths")]
        new: Vec<RepoPath>,
    },
    /// Commit or amend on top of `parent`.
    Commit {
        /// Parent changeset.
        parent: ObjectId,
        /// Change id of the new changeset.
        change_id: ChangeId,
        /// True for amend.
        amend: bool,
    },
    /// Stack rebase.
    Rebase {
        /// Original head.
        original: ObjectId,
        /// Destination.
        onto: ObjectId,
        /// Members still to rebase.
        remaining_stack: Vec<ObjectId>,
        /// Rebased results so far.
        done: Vec<ObjectId>,
        /// Conflicting paths (raw bytes).
        conflicts: Vec<Vec<u8>>,
        /// Resolved paths (raw bytes).
        resolved: Vec<Vec<u8>>,
        /// Member being rebased.
        current: Option<ObjectId>,
    },
}

/// Working-copy state (SPEC 4.3.10).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WcState {
    /// Endpoint override.
    pub endpoint: Option<String>,
    /// Base changeset.
    pub base: ObjectId,
    /// Tracked ref.
    #[serde(default, with = "crate::serde_util::opt_ref")]
    pub tracking_ref: Option<RefName>,
    /// Sparse profile prefixes.
    #[serde(with = "crate::serde_util::paths")]
    pub sparse: Vec<RepoPath>,
    /// Prefixes the caller cannot read.
    #[serde(with = "crate::serde_util::paths")]
    pub inaccessible: Vec<RepoPath>,
    /// Journaled operation, if any.
    pub pending: Option<PendingOperation>,
}

/// Load `<scion_dir>/state` (SPEC 11.1). A missing file is NOT_FOUND ("not a working copy");
/// unparsable content is INVALID_ARGUMENT.
pub fn load(scion_dir: &Path) -> Result<WcState> {
    let path = scion_dir.join(STATE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ScionError::not_found(format!(
                "no working-copy state at {} (not a scion working copy?)",
                path.display()
            )));
        }
        Err(e) => return Err(io_error("read .scion/state", e)),
    };
    toml::from_str(&text).map_err(|e| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("corrupt .scion/state: {}", e.message()),
        )
    })
}

/// Save `<scion_dir>/state` as canonical TOML, atomically (SPEC 11.1, 11.3).
pub fn save(scion_dir: &Path, s: &WcState) -> Result<()> {
    let text = to_canonical_toml(s)?;
    write_atomic(scion_dir, STATE_FILE, text.as_bytes(), 0o644, true)
}

/// The canonical TOML text of a state (keys sorted at every level; PLAN 6.15).
pub fn to_canonical_toml(s: &WcState) -> Result<String> {
    let err = |e: &dyn std::fmt::Display| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("cannot serialize working-copy state: {e}"),
        )
    };
    let value = toml::Value::try_from(s).map_err(|e| err(&e))?;
    toml::to_string(&canonical(value)).map_err(|e| err(&e))
}

/// Rebuild every table with its keys in sorted order.
fn canonical(v: toml::Value) -> toml::Value {
    match v {
        toml::Value::Table(t) => {
            let mut items: Vec<(String, toml::Value)> = t.into_iter().collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = toml::map::Map::new();
            for (k, v) in items {
                out.insert(k, canonical(v));
            }
            toml::Value::Table(out)
        }
        toml::Value::Array(a) => toml::Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{Kind, empty_tree_id};

    fn sample() -> WcState {
        WcState {
            endpoint: Some("unix:///run/scion/daemon.sock".into()),
            base: ObjectId::new(Kind::Changeset, [7; 32]),
            tracking_ref: None,
            sparse: Vec::new(),
            inaccessible: Vec::new(),
            pending: Some(PendingOperation::Rebase {
                original: ObjectId::new(Kind::Changeset, [1; 32]),
                onto: ObjectId::new(Kind::Changeset, [2; 32]),
                remaining_stack: vec![ObjectId::new(Kind::Changeset, [3; 32])],
                done: vec![],
                conflicts: vec![b"a/b".to_vec(), vec![0xff, 0]],
                resolved: vec![],
                current: Some(empty_tree_id()),
            }),
        }
    }

    #[test]
    fn round_trip_canonical_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap_err().code, ErrorCode::NotFound);
        let s = sample();
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap(), s);
        let first = std::fs::read(dir.path().join(STATE_FILE)).unwrap();
        save(dir.path(), &s).unwrap();
        assert_eq!(std::fs::read(dir.path().join(STATE_FILE)).unwrap(), first);
        // Top-level keys are sorted.
        let text = String::from_utf8(first).unwrap();
        let top: Vec<&str> = text
            .lines()
            .take_while(|l| !l.starts_with('['))
            .filter_map(|l| l.split(" = ").next())
            .filter(|k| !k.is_empty())
            .collect();
        let mut sorted = top.clone();
        sorted.sort();
        assert_eq!(top, sorted);
        assert!(top.len() >= 4, "{text}");
        // Only the state file is left in the directory (no temporaries).
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let commit = WcState {
            pending: Some(PendingOperation::Commit {
                parent: s.base,
                change_id: ChangeId([9; 16]),
                amend: true,
            }),
            endpoint: None,
            ..s.clone()
        };
        save(dir.path(), &commit).unwrap();
        assert_eq!(load(dir.path()).unwrap(), commit);
        let none = WcState {
            pending: None,
            ..commit
        };
        save(dir.path(), &none).unwrap();
        assert_eq!(load(dir.path()).unwrap(), none);

        std::fs::write(dir.path().join(STATE_FILE), "base = 3\n").unwrap();
        assert_eq!(
            load(dir.path()).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn crash_before_rename_keeps_old_state() {
        let dir = tempfile::tempdir().unwrap();
        let s = sample();
        save(dir.path(), &s).unwrap();
        // A crash after writing a temporary file but before the rename leaves debris only.
        let mut new = s.clone();
        new.pending = None;
        let text = to_canonical_toml(&new).unwrap();
        std::fs::write(
            dir.path().join(".state.tmp-crashed"),
            &text.as_bytes()[..text.len() / 2],
        )
        .unwrap();
        assert_eq!(load(dir.path()).unwrap(), s);
        // The next save still succeeds and wins.
        save(dir.path(), &new).unwrap();
        assert_eq!(load(dir.path()).unwrap(), new);
    }
}
