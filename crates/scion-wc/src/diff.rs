//! Working-copy diff (P18): the files that differ from the base within scope (SPEC 11.5),
//! with old and new blob ids and modes, and optionally unified content hunks.

use crate::remote::Item;
use crate::wc::WorkingCopy;
use scion_core::{EntryInfo, ErrorCode, Limits, Mode, ObjectId, Result, ScionError};
use scion_object::RepoPath;
use std::collections::{BTreeMap, HashMap};

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
    /// Either side is binary (a NUL in its first 8000 bytes, or larger than
    /// `merge_max_text_bytes`); no hunks are produced then. Always `false` without hunks.
    pub binary: bool,
    /// Unified content hunks (`diff_with_hunks` only); `None` when not requested, for binary
    /// changes, and when a side's content is unavailable (redacted or denied).
    pub hunks: Option<Vec<HunkView>>,
}

/// One unified-diff hunk (1-based starts as in `@@ -a,b +c,d @@`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct HunkView {
    /// Start line in the old text.
    pub old_start: u32,
    /// Line count in the old text.
    pub old_lines: u32,
    /// Start line in the new text.
    pub new_start: u32,
    /// Line count in the new text.
    pub new_lines: u32,
    /// The hunk's lines in order.
    pub lines: Vec<HunkLineView>,
}

/// One line of a [`HunkView`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct HunkLineView {
    /// `context`, `delete` or `insert`.
    pub kind: String,
    /// The line including its terminator (lossy UTF-8).
    pub text: String,
    /// Hex of the exact line bytes, present only when the line is not valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_hex: Option<String>,
}

impl HunkLineView {
    /// The exact line bytes (from `text_hex` when present, else `text`).
    pub fn bytes(&self) -> Vec<u8> {
        self.text_hex
            .as_deref()
            .and_then(|h| hex::decode(h).ok())
            .unwrap_or_else(|| self.text.as_bytes().to_vec())
    }
}

impl From<scion_merge::TextHunk> for HunkView {
    fn from(h: scion_merge::TextHunk) -> Self {
        HunkView {
            old_start: h.old_start,
            old_lines: h.old_lines,
            new_start: h.new_start,
            new_lines: h.new_lines,
            lines: h
                .lines
                .into_iter()
                .map(|(tag, line)| {
                    let kind = match tag {
                        b'-' => "delete",
                        b'+' => "insert",
                        _ => "context",
                    };
                    let (text, text_hex) = match String::from_utf8(line) {
                        Ok(t) => (t, None),
                        Err(e) => {
                            let raw = e.into_bytes();
                            (
                                String::from_utf8_lossy(&raw).into_owned(),
                                Some(hex::encode(&raw)),
                            )
                        }
                    };
                    HunkLineView {
                        kind: kind.to_owned(),
                        text,
                        text_hex,
                    }
                })
                .collect(),
        }
    }
}

/// One change found by the status walk.
struct Change {
    path: RepoPath,
    status: &'static str,
    old: Option<EntryInfo>,
    new: Option<EntryInfo>,
    /// The new content read from disk (only when hunks are requested).
    content: Option<Vec<u8>>,
}

impl WorkingCopy {
    /// Working-copy changes against the base (SPEC 11.5), sorted by path. Uses the local tree
    /// cache, so it makes no server call when every needed tree is cached.
    pub async fn diff(&mut self) -> Result<Vec<FileChangeView>> {
        let _l = self.lock()?;
        let changes = self.changes(false).await?;
        Ok(changes
            .iter()
            .map(|c| view(&c.path, c.status, c.old, c.new))
            .collect())
    }

    /// [`WorkingCopy::diff`] with unified content hunks (`context` lines of context), using the
    /// same rules as the server's `Diff(..., include_content_hunks)` (SPEC 7.4): a change whose
    /// sides are binary (a NUL in the first 8000 bytes, or larger than `merge_max_text_bytes`)
    /// sets `binary` and has no hunks; a redacted or denied base blob yields no hunks. Base blobs
    /// are fetched in batches; new content comes from disk (symlinks diff their targets).
    pub async fn diff_with_hunks(&mut self, context: usize) -> Result<Vec<FileChangeView>> {
        let _l = self.lock()?;
        let changes = self.changes(true).await?;
        let max = Limits::default().merge_max_text_bytes;
        let too_big = |c: &Change| [c.old, c.new].iter().flatten().any(|e| e.size > max);
        let same = |c: &Change| c.old.map(|e| e.id) == c.new.map(|e| e.id);
        let ids: Vec<ObjectId> = changes
            .iter()
            .filter(|c| !same(c) && !too_big(c))
            .filter_map(|c| c.old.map(|e| e.id))
            .collect();
        let mut blobs: HashMap<ObjectId, Item> = HashMap::new();
        for chunk in ids.chunks(self.opts.fetch_batch.max(1)) {
            blobs.extend(self.fetcher.get_objects(chunk).await?);
        }
        let mut out = Vec::with_capacity(changes.len());
        for c in changes {
            let mut v = view(&c.path, c.status, c.old, c.new);
            if same(&c) {
                v.hunks = Some(Vec::new());
            } else if too_big(&c) {
                v.binary = true;
            } else {
                let old: Option<&[u8]> = match c.old {
                    None => Some(&[]),
                    Some(e) => match blobs.get(&e.id) {
                        Some(Item::Ok(b)) => Some(b),
                        Some(Item::Redacted | Item::Denied) => None,
                        Some(Item::NotFound) | None => {
                            return Err(ScionError::not_found(format!(
                                "blob {} not found upstream",
                                e.id
                            )));
                        }
                    },
                };
                let new: &[u8] = c.content.as_deref().unwrap_or_default();
                if let Some(old) = old {
                    if scion_merge::is_binary(old) || scion_merge::is_binary(new) {
                        v.binary = true;
                    } else {
                        v.hunks = Some(
                            scion_merge::unified_hunks(old, new, context)
                                .into_iter()
                                .map(HunkView::from)
                                .collect(),
                        );
                    }
                }
            }
            out.push(v);
        }
        Ok(out)
    }

    /// The status walk as changes sorted by path (caller holds the lock); with `content`, the
    /// new side's bytes are kept.
    async fn changes(&mut self, content: bool) -> Result<Vec<Change>> {
        if self.state.pending.is_some() {
            self.recover_inner().await?;
        }
        let mut idx = self.load_index()?;
        let report = self.status_inner(&mut idx).await?;
        self.save_index(&mut idx)?;
        let base = self.base_listing().await?.files;
        let mut out: BTreeMap<RepoPath, Change> = BTreeMap::new();
        let groups: [(&'static str, &Vec<RepoPath>); 4] = [
            ("modified", &report.modified),
            ("added", &report.added),
            ("deleted", &report.deleted),
            ("mode_changed", &report.mode_changed),
        ];
        for (status, paths) in groups {
            for p in paths {
                let old = base.get(p).copied();
                let (new, bytes) = if status == "deleted" {
                    (None, None)
                } else {
                    match self.disk_content(p)? {
                        Some((e, b)) => (Some(e), content.then_some(b)),
                        None => (None, None),
                    }
                };
                out.insert(
                    p.clone(),
                    Change {
                        path: p.clone(),
                        status,
                        old,
                        new,
                        content: bytes,
                    },
                );
            }
        }
        Ok(out.into_values().collect())
    }

    /// The current disk state of `p` as an entry (blob id of its content or symlink target),
    /// with that content.
    fn disk_content(&self, p: &RepoPath) -> Result<Option<(EntryInfo, Vec<u8>)>> {
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
            Ok(c) => Ok(Some((
                EntryInfo {
                    mode,
                    id: ObjectId::hash_blob(&c),
                    size: c.len() as u64,
                },
                c,
            ))),
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
        binary: false,
        hunks: None,
    }
}
