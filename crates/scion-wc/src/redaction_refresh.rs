//! SPEC 13.7 replacement of newly redacted clean files with placeholders (P18).
//!
//! The base tree's in-scope blobs that are materialized with real content are probed with
//! GetObjects (zero-length range reads, so no content is transferred); every blob whose status
//! is `redacted` has its clean copies replaced by the placeholder and marked `redacted` in the
//! index. Dirty copies are never touched; they are returned (reported).

use crate::index::IndexEntry;
use crate::placeholder::placeholder_bytes;
use crate::wc::WorkingCopy;
use scion_core::{EntryInfo, ErrorCode, Mode, ObjectId, Result};
use scion_object::RepoPath;
use scion_proto::v1;
use std::collections::BTreeMap;

impl WorkingCopy {
    /// Replace clean materialized copies of newly redacted blobs with placeholders; returns the
    /// dirty files that were left untouched (reported). SPEC 13.7.
    pub async fn refresh_redactions(&mut self) -> Result<Vec<RepoPath>> {
        let _l = self.lock()?;
        self.recover_inner().await?;
        self.refresh_redactions_inner().await
    }

    pub(crate) async fn refresh_redactions_inner(&mut self) -> Result<Vec<RepoPath>> {
        let listing = self.base_listing().await?;
        let mut idx = self.load_index()?;
        let mut by_blob: BTreeMap<ObjectId, Vec<(RepoPath, EntryInfo)>> = BTreeMap::new();
        for (p, e) in &listing.files {
            if idx.entries.get(p).is_some_and(|ie| ie.redacted) {
                continue;
            }
            if self.safe.stat(p)?.is_none() {
                continue;
            }
            by_blob.entry(e.id).or_default().push((p.clone(), *e));
        }
        if by_blob.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<ObjectId> = by_blob.keys().copied().collect();
        let statuses = self.fetcher.probe_status(&ids).await?;
        let mut dirty = Vec::new();
        let mut replaced = 0usize;
        for (id, status) in statuses {
            if status != v1::ItemStatus::Redacted {
                continue;
            }
            for (path, e) in by_blob.remove(&id).unwrap_or_default() {
                if self.is_clean_copy(&idx, &path, &e)? {
                    let mode = if e.mode == Mode::Executable {
                        Mode::Executable
                    } else {
                        Mode::File
                    };
                    self.safe
                        .write_file_atomic(&path, &placeholder_bytes(&e.id), mode)?;
                    match self.safe.stat(&path)? {
                        Some(stat) => {
                            idx.entries.insert(
                                path,
                                IndexEntry {
                                    stat,
                                    id: e.id,
                                    mode: e.mode,
                                    redacted: true,
                                },
                            );
                        }
                        None => {
                            idx.entries.remove(&path);
                        }
                    }
                    replaced += 1;
                } else {
                    dirty.push(path);
                }
            }
        }
        if replaced > 0 {
            tracing::info!(replaced, "replaced newly redacted files with placeholders");
            self.save_index(&mut idx)?;
        }
        dirty.sort();
        Ok(dirty)
    }

    /// Whether the materialized file at `path` is an unmodified copy of `e` (trusted by stat
    /// when not racily clean, otherwise re-hashed; SPEC 11.5).
    fn is_clean_copy(
        &self,
        idx: &crate::index::StatCache,
        path: &RepoPath,
        e: &EntryInfo,
    ) -> Result<bool> {
        let Some(st) = self.safe.stat(path)? else {
            return Ok(false);
        };
        if let Some(ie) = idx.entries.get(path)
            && ie.stat == st
            && !idx.is_racy(ie)
        {
            return Ok(ie.id == e.id && ie.mode == e.mode);
        }
        let Some(disk_mode) = st.entry_mode() else {
            return Ok(false);
        };
        if disk_mode != e.mode {
            return Ok(false);
        }
        let content = match disk_mode {
            Mode::Symlink => self.safe.read_link(path),
            _ => self.safe.read(path),
        };
        match content {
            Ok(c) => Ok(ObjectId::hash_blob(&c) == e.id),
            Err(err) if err.code == ErrorCode::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }
}
