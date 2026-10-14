//! Merge conflict payload types (SPEC 8.3). Paths are raw bytes so that `scion-core` stays
//! below `scion-object`.

use crate::id::ObjectId;
use crate::mode::Mode;
use std::ops::Range;

/// A file-level tree entry: `(mode, id, size)` (SPEC 4.3.2, 8.3).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryInfo {
    /// Entry mode.
    pub mode: Mode,
    /// Entry object id.
    pub id: ObjectId,
    /// Blob length for modes 1-3; 0 for mode 4.
    pub size: u64,
}

/// SPEC 8.3 conflict kinds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    /// Both sides added different content at a path absent in the base.
    AddAdd,
    /// One side deleted, the other modified.
    ModifyDelete,
    /// A symlink side differs.
    Symlink,
    /// Binary or oversized content.
    Binary,
    /// A needed side's content is redacted.
    Redacted,
    /// A structural file/directory conflict.
    FileDirectory,
    /// Overlapping line hunks (line-diff3-v1).
    Content,
}

impl ConflictKind {
    /// Every kind, in SPEC 8.3 order.
    pub const ALL: [ConflictKind; 7] = [
        ConflictKind::AddAdd,
        ConflictKind::ModifyDelete,
        ConflictKind::Symlink,
        ConflictKind::Binary,
        ConflictKind::Redacted,
        ConflictKind::FileDirectory,
        ConflictKind::Content,
    ];

    /// Wire string: `add_add|modify_delete|symlink|binary|redacted|file_directory|content`.
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictKind::AddAdd => "add_add",
            ConflictKind::ModifyDelete => "modify_delete",
            ConflictKind::Symlink => "symlink",
            ConflictKind::Binary => "binary",
            ConflictKind::Redacted => "redacted",
            ConflictKind::FileDirectory => "file_directory",
            ConflictKind::Content => "content",
        }
    }

    /// Inverse of [`ConflictKind::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        ConflictKind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// A conflicting line region: base/ours/theirs line ranges (0-based, half-open).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHunk {
    /// Lines of the base.
    pub base: Range<u32>,
    /// Lines of ours.
    pub ours: Range<u32>,
    /// Lines of theirs.
    pub theirs: Range<u32>,
}

/// One conflicting path (SPEC 8.3 `{path, kind, base?, ours?, theirs?, hunks?}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Repository path bytes.
    pub path: Vec<u8>,
    /// Conflict kind.
    pub kind: ConflictKind,
    /// Base entry, if present.
    pub base: Option<EntryInfo>,
    /// Ours entry, if present.
    pub ours: Option<EntryInfo>,
    /// Theirs entry, if present.
    pub theirs: Option<EntryInfo>,
    /// Content hunks (only for [`ConflictKind::Content`]).
    pub hunks: Vec<ConflictHunk>,
}

/// A cleanly merged path; `entry == None` means deleted (SPEC 8.3 `clean_entries`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanEntry {
    /// Repository path bytes.
    pub path: Vec<u8>,
    /// Resulting entry, or `None` when deleted.
    pub entry: Option<EntryInfo>,
}

/// MERGE_CONFLICT payload: clean entries for every non-conflicting path plus the conflicts.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConflictReport {
    /// Non-conflicting paths and their merged entries.
    pub clean_entries: Vec<CleanEntry>,
    /// Conflicting paths.
    pub conflicts: Vec<Conflict>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_round_trip() {
        for k in ConflictKind::ALL {
            assert_eq!(ConflictKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ConflictKind::parse("nope"), None);
        assert_eq!(ConflictKind::FileDirectory.as_str(), "file_directory");
    }

    #[test]
    fn report_default_is_empty() {
        let r = ConflictReport::default();
        assert!(r.clean_entries.is_empty() && r.conflicts.is_empty());
    }
}
