//! SPEC 11.5 / 18.7 status over a base-tree lookup (P11).
//!
//! The walk starts at the root descriptor of a [`SafeRoot`] and opens every directory with
//! `O_NOFOLLOW`, so symlinked directories are reported as symlinks and never descended into.
//! It visits only directories that are in scope or lead to a scope prefix (SPEC 11.2), skips
//! `.scion` (in any case), inaccessible prefixes (SPEC 12.6), ignored untracked files and
//! ignored directories without tracked files (SPEC 11.4).
//!
//! A file is hashed only when it has no index entry, its stat tuple changed, or the entry is
//! racily clean (`mtime >= written_at`); symlinks hash their target. Clean files are
//! refreshed in the index; entries of in-scope paths that vanished are dropped. Redaction
//! placeholders (SPEC 13.7): a regular file whose bytes are exactly the placeholder of the
//! base entry's blob is clean (and marked `redacted` in the index); an edited placeholder (an
//! index entry flagged `redacted` whose content changed) is reported in both `modified` and
//! `redacted_dirty`. Saving the index is the caller's job.

use crate::fs_safe::{self, SafeRoot, check_name};
use crate::ignore::IgnoreMatcher;
use crate::index::{IndexEntry, StatCache, StatTuple};
use crate::placeholder::placeholder_bytes;
use rustix::fd::{AsFd, BorrowedFd};
use rustix::io::Errno;
use scion_core::{EntryInfo, ErrorCode, Mode, ObjectId, Result};
use scion_object::RepoPath;
use std::collections::{BTreeMap, BTreeSet};

/// Base tree access for status (implemented over the local tree cache; SPEC 11.5).
pub trait TreeLookup: Send + Sync {
    /// Every file-level entry of the base tree under the scope prefixes.
    fn files_in_scope(&self, scope: &[RepoPath]) -> Result<BTreeMap<RepoPath, EntryInfo>>;
}

/// Status result (SPEC 11.5). Every list is sorted bytewise.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct StatusReport {
    /// Content changed.
    #[serde(with = "crate::serde_util::paths")]
    pub modified: Vec<RepoPath>,
    /// Not in the base tree.
    #[serde(with = "crate::serde_util::paths")]
    pub added: Vec<RepoPath>,
    /// In the base tree, missing on disk.
    #[serde(with = "crate::serde_util::paths")]
    pub deleted: Vec<RepoPath>,
    /// Same content, different mode.
    #[serde(with = "crate::serde_util::paths")]
    pub mode_changed: Vec<RepoPath>,
    /// Edited redaction placeholders (also listed in `modified`).
    #[serde(with = "crate::serde_util::paths")]
    pub redacted_dirty: Vec<RepoPath>,
    /// Number of files hashed.
    pub hashed: usize,
}

impl StatusReport {
    /// True when nothing changed (SPEC 11.5).
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty()
            && self.added.is_empty()
            && self.deleted.is_empty()
            && self.mode_changed.is_empty()
            && self.redacted_dirty.is_empty()
    }
}

/// Byte-level prefix sets (SPEC 11.2 scope, SPEC 12.6 inaccessible).
pub(crate) struct Prefixes {
    scope: Vec<Vec<u8>>,
    inaccessible: Vec<Vec<u8>>,
}

/// `prefix` equals `p` or is a component-wise ancestor of it (the root is everyone's).
fn covers(prefix: &[u8], p: &[u8]) -> bool {
    prefix.is_empty()
        || p == prefix
        || (p.len() > prefix.len() && p.starts_with(prefix) && p[prefix.len()] == b'/')
}

impl Prefixes {
    pub(crate) fn new(scope: &[RepoPath], inaccessible: &[RepoPath]) -> Self {
        Prefixes {
            scope: scope.iter().map(|p| p.as_bytes().to_vec()).collect(),
            inaccessible: inaccessible.iter().map(|p| p.as_bytes().to_vec()).collect(),
        }
    }

    /// In scope: a profile entry equals it or is an ancestor (SPEC 11.2).
    pub(crate) fn in_scope(&self, p: &[u8]) -> bool {
        self.scope.iter().any(|s| covers(s, p))
    }

    /// A directory that is a strict ancestor of some scope prefix (walked, not in scope).
    pub(crate) fn leads_to_scope(&self, dir: &[u8]) -> bool {
        self.scope
            .iter()
            .any(|s| s.len() > dir.len() && covers(dir, s))
    }

    /// At or under an inaccessible prefix (SPEC 12.6).
    pub(crate) fn inaccessible(&self, p: &[u8]) -> bool {
        self.inaccessible.iter().any(|i| covers(i, p))
    }
}

struct Walk<'a> {
    idx: &'a mut StatCache,
    ign: &'a IgnoreMatcher,
    prefixes: Prefixes,
    base: BTreeMap<RepoPath, EntryInfo>,
    base_dirs: BTreeSet<Vec<u8>>,
    seen: BTreeSet<RepoPath>,
    report: StatusReport,
}

/// Walk in-scope files (skipping `.scion`, ignored files and inaccessible prefixes), hash only
/// changed or racy entries, and compare with `base` (SPEC 11.5, 18.7).
pub fn status(
    root: &SafeRoot,
    idx: &mut StatCache,
    ign: &IgnoreMatcher,
    scope: &[RepoPath],
    inaccessible: &[RepoPath],
    base: &dyn TreeLookup,
) -> Result<StatusReport> {
    let prefixes = Prefixes::new(scope, inaccessible);
    let mut base_files = base.files_in_scope(scope)?;
    base_files
        .retain(|p, _| prefixes.in_scope(p.as_bytes()) && !prefixes.inaccessible(p.as_bytes()));
    let mut base_dirs = BTreeSet::new();
    for p in base_files.keys() {
        let b = p.as_bytes();
        for (k, c) in b.iter().enumerate() {
            if *c == b'/' {
                base_dirs.insert(b[..k].to_vec());
            }
        }
    }
    let mut w = Walk {
        idx,
        ign,
        prefixes,
        base: base_files,
        base_dirs,
        seen: BTreeSet::new(),
        report: StatusReport::default(),
    };
    if !w.prefixes.scope.is_empty() {
        w.walk_dir(root.root_fd(), &[])?;
    }

    // Deleted: base files in scope not seen on disk.
    let mut deleted = Vec::new();
    for p in w.base.keys() {
        if !w.seen.contains(p) {
            deleted.push(p.clone());
        }
    }
    // Drop index entries of in-scope, accessible paths that are no longer on disk.
    let Walk {
        idx,
        prefixes,
        seen,
        mut report,
        ..
    } = w;
    idx.entries.retain(|p, _| {
        let b = p.as_bytes();
        seen.contains(p) || !prefixes.in_scope(b) || prefixes.inaccessible(b)
    });
    report.deleted = deleted;
    for v in [
        &mut report.modified,
        &mut report.added,
        &mut report.deleted,
        &mut report.mode_changed,
        &mut report.redacted_dirty,
    ] {
        v.sort();
        v.dedup();
    }
    Ok(report)
}

impl Walk<'_> {
    fn walk_dir(&mut self, dir: BorrowedFd<'_>, rel: &[u8]) -> Result<()> {
        for name in fs_safe::list_dir(dir)? {
            if name.eq_ignore_ascii_case(b".scion") {
                continue;
            }
            if check_name(&name).is_err() {
                tracing::debug!("skipping a directory entry whose name violates SPEC 4.4");
                continue;
            }
            let child = join(rel, &name);
            if child.len() > 4096 || self.prefixes.inaccessible(&child) {
                continue;
            }
            let Some(st) = fs_safe::stat_at(dir, &name)? else {
                continue; // vanished
            };
            let tuple = fs_safe::tuple_from_stat(&st);
            if tuple.is_dir() {
                self.visit_dir(dir, &name, &child)?;
            } else if let Some(mode) = tuple.entry_mode()
                && self.prefixes.in_scope(&child)
            {
                self.visit_file(dir, &name, &child, tuple, mode)?;
            }
        }
        Ok(())
    }

    fn visit_dir(&mut self, dir: BorrowedFd<'_>, name: &[u8], child: &[u8]) -> Result<()> {
        if !self.prefixes.in_scope(child) && !self.prefixes.leads_to_scope(child) {
            return Ok(());
        }
        if !self.base_dirs.contains(child) && self.ign.is_ignored_bytes(child, true) {
            return Ok(());
        }
        let sub = match fs_safe::open_dir_nofollow(dir, name) {
            Ok(fd) => fd,
            // Replaced by a symlink or removed mid-walk, or unreadable: nothing to report here.
            Err(Errno::NOENT) | Err(Errno::LOOP) | Err(Errno::NOTDIR) | Err(Errno::ACCESS) => {
                return Ok(());
            }
            Err(e) => {
                return Err(fs_safe::io_error("open directory", std::io::Error::from(e)));
            }
        };
        self.walk_dir(sub.as_fd(), child)
    }

    fn visit_file(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &[u8],
        child: &[u8],
        stat: StatTuple,
        disk_mode: Mode,
    ) -> Result<()> {
        let Ok(path) = RepoPath::parse(child) else {
            return Ok(());
        };
        let base = self.base.get(&path).copied();
        if base.is_none() && self.ign.is_ignored_bytes(child, false) {
            return Ok(());
        }

        // Trusted by stat: same tuple and not racily clean.
        if let Some(e) = self.idx.entries.get(&path)
            && e.stat == stat
            && !self.idx.is_racy(e)
        {
            let (id, mode, redacted) = (e.id, e.mode, e.redacted);
            self.seen.insert(path.clone());
            match base {
                None => self.report.added.push(path),
                Some(b) if b.id != id => self.report.modified.push(path),
                Some(b) if !redacted && b.mode != mode => self.report.mode_changed.push(path),
                Some(_) => {}
            }
            return Ok(());
        }

        let content = match disk_mode {
            Mode::Symlink => fs_safe::readlink_at(dir, name),
            _ => fs_safe::read_file_at(dir, name),
        };
        let content = match content {
            Ok(c) => c,
            Err(e) if e.code == ErrorCode::NotFound => return Ok(()), // vanished
            Err(e) => return Err(e),
        };
        self.report.hashed += 1;
        let h = ObjectId::hash_blob(&content);
        self.seen.insert(path.clone());
        let was_redacted = self
            .idx
            .entries
            .get(&path)
            .map(|e| e.redacted)
            .unwrap_or(false);
        let Some(b) = base else {
            self.report.added.push(path);
            return Ok(());
        };
        if disk_mode != Mode::Symlink && b.id != h && content == placeholder_bytes(&b.id) {
            // An unmodified placeholder of the redacted base blob (SPEC 13.7).
            self.refresh(path, stat, b.id, b.mode, true);
        } else if b.id != h {
            if was_redacted {
                self.report.redacted_dirty.push(path.clone());
            }
            self.report.modified.push(path);
        } else if b.mode != disk_mode {
            self.report.mode_changed.push(path);
        } else {
            self.refresh(path, stat, h, disk_mode, false);
        }
        Ok(())
    }

    fn refresh(
        &mut self,
        path: RepoPath,
        stat: StatTuple,
        id: ObjectId,
        mode: Mode,
        redacted: bool,
    ) {
        self.idx.entries.insert(
            path,
            IndexEntry {
                stat,
                id,
                mode,
                redacted,
            },
        );
    }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        let mut v = Vec::with_capacity(dir.len() + 1 + name.len());
        v.extend_from_slice(dir);
        v.push(b'/');
        v.extend_from_slice(name);
        v
    }
}

/// A [`TreeLookup`] over an in-memory map, filtered by scope (tests and simple callers).
#[derive(Clone, Debug, Default)]
pub struct MapTreeLookup(pub BTreeMap<RepoPath, EntryInfo>);

impl TreeLookup for MapTreeLookup {
    fn files_in_scope(&self, scope: &[RepoPath]) -> Result<BTreeMap<RepoPath, EntryInfo>> {
        let prefixes = Prefixes::new(scope, &[]);
        Ok(self
            .0
            .iter()
            .filter(|(p, _)| prefixes.in_scope(p.as_bytes()))
            .map(|(p, e)| (p.clone(), *e))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_rules() {
        let p = Prefixes {
            scope: vec![b"a/b".to_vec(), b"c".to_vec()],
            inaccessible: vec![b"c/secret".to_vec()],
        };
        assert!(p.in_scope(b"a/b"));
        assert!(p.in_scope(b"a/b/x"));
        assert!(!p.in_scope(b"a/bx"));
        assert!(!p.in_scope(b"a"));
        assert!(p.leads_to_scope(b"a"));
        assert!(!p.leads_to_scope(b"a/b"));
        assert!(!p.leads_to_scope(b"ab"));
        assert!(p.in_scope(b"c/secret/x"));
        assert!(p.inaccessible(b"c/secret/x"));
        assert!(p.inaccessible(b"c/secret"));
        assert!(!p.inaccessible(b"c/secretx"));
        let all = Prefixes {
            scope: vec![Vec::new()],
            inaccessible: vec![],
        };
        assert!(all.in_scope(b"anything/at/all"));
        assert!(!all.leads_to_scope(b"x"));
    }
}
