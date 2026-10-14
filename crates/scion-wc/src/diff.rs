//! Working-copy diff (P18): the files that differ from the base within scope (SPEC 11.5),
//! with old and new blob ids and modes.

use crate::wc::WorkingCopy;
use scion_core::{EntryInfo, ErrorCode, Mode, ObjectId, Result};
use scion_object::RepoPath;
use std::collections::BTreeMap;

/// One changed file (`--json` schema `scion.cli.diff.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct FileChangeView {
    /// Path (lossy UTF-8 for display).
    pub path: String,
    /// `modified`, `added`, `deleted` or `mode_changed`.
    pub status: String,
    /// Old blob id text.
    pub old_id: Option<String>,
    /// New blob id text.
    pub new_id: Option<String>,
    /// Old mode (1-3).
    pub old_mode: Option<u8>,
    /// New mode (1-3).
    pub new_mode: Option<u8>,
}

impl WorkingCopy {
    /// Working-copy changes against the base (SPEC 11.5), sorted by path. Uses the local tree
    /// cache, so it makes no server call when every needed tree is cached.
    pub async fn diff(&mut self) -> Result<Vec<FileChangeView>> {
        let _l = self.lock()?;
        if self.state.pending.is_some() {
            self.recover_inner().await?;
        }
        let mut idx = self.load_index()?;
        let report = self.status_inner(&mut idx).await?;
        self.save_index(&mut idx)?;
        let base = self.base_listing().await?.files;
        let mut out: BTreeMap<RepoPath, FileChangeView> = BTreeMap::new();
        let groups: [(&str, &Vec<RepoPath>); 4] = [
            ("modified", &report.modified),
            ("added", &report.added),
            ("deleted", &report.deleted),
            ("mode_changed", &report.mode_changed),
        ];
        for (status, paths) in groups {
            for p in paths {
                let old = base.get(p).copied();
                let new = if status == "deleted" {
                    None
                } else {
                    self.disk_entry(p)?
                };
                out.insert(p.clone(), view(p, status, old, new));
            }
        }
        Ok(out.into_values().collect())
    }

    /// The current disk state of `p` as an entry (blob id of its content or symlink target).
    fn disk_entry(&self, p: &RepoPath) -> Result<Option<EntryInfo>> {
        let Some(st) = self.safe.stat(p)? else {
            return Ok(None);
        };
        let Some(mode) = st.entry_mode() else {
            return Ok(None);
        };
        let content = match mode {
            Mode::Symlink => self.safe.read_link(p),
            _ => self.safe.read(p),
        };
        match content {
            Ok(c) => Ok(Some(EntryInfo {
                mode,
                id: ObjectId::hash_blob(&c),
                size: c.len() as u64,
            })),
            Err(e) if e.code == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn view(
    p: &RepoPath,
    status: &str,
    old: Option<EntryInfo>,
    new: Option<EntryInfo>,
) -> FileChangeView {
    FileChangeView {
        path: String::from_utf8_lossy(p.as_bytes()).into_owned(),
        status: status.to_owned(),
        old_id: old.map(|e| e.id.to_text()),
        new_id: new.map(|e| e.id.to_text()),
        old_mode: old.map(|e| e.mode as u8),
        new_mode: new.map(|e| e.mode as u8),
    }
}
