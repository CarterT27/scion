//! SPEC 11.7 conflict resolution (P28).
//!
//! `resolve <path>` marks a conflicting path of a paused rebase resolved. It refuses while
//! conflict marker lines remain in the file (a line starting with `<<<<<<<`, `|||||||` or
//! `>>>>>>>`, the markers `scion_merge::render_markers` writes), unless `force`. A lone
//! `=======` line is not treated as a marker, so ordinary text (for example a Markdown setext
//! heading underline) never blocks resolution. A deleted file is a valid resolution.

use crate::state::PendingOperation;
use crate::wc::WorkingCopy;
use scion_core::{Mode, Result, ScionError};
use scion_object::RepoPath;

/// Whether `content` still holds a conflict marker line (SPEC 11.7).
pub(crate) fn has_marker_lines(content: &[u8]) -> bool {
    content.split(|c| *c == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        [&b"<<<<<<<"[..], b"|||||||", b">>>>>>>"].iter().any(|m| {
            line.starts_with(m) && matches!(line.get(m.len()), None | Some(b' ') | Some(b'\t'))
        })
    })
}

impl WorkingCopy {
    /// INVALID_ARGUMENT subcode: no rebase is paused on conflicts (SPEC 11.7).
    pub const SUBCODE_NO_REBASE: &'static str = "no_rebase_in_progress";
    /// INVALID_ARGUMENT subcode: the path is not one of the rebase conflicts (SPEC 11.7).
    pub const SUBCODE_NOT_CONFLICTED: &'static str = "not_conflicted";
    /// INVALID_ARGUMENT subcode: conflict markers remain in the file (SPEC 11.7).
    pub const SUBCODE_MARKERS_REMAIN: &'static str = "conflict_markers_remain";

    /// Mark `p` resolved; refuses while marker lines remain unless `force` (SPEC 11.7).
    pub fn resolve(&mut self, p: &RepoPath, force: bool) -> Result<()> {
        let _l = self.lock()?;
        let is_conflict = match &self.state.pending {
            Some(PendingOperation::Rebase { conflicts, .. }) if !conflicts.is_empty() => {
                conflicts.iter().any(|c| c.as_slice() == p.as_bytes())
            }
            _ => {
                return Err(ScionError::invalid(
                    WorkingCopy::SUBCODE_NO_REBASE,
                    "no rebase is paused on conflicts",
                ));
            }
        };
        if !is_conflict {
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_NOT_CONFLICTED,
                "the path is not a conflict of the paused rebase",
            ));
        }
        if !force && self.markers_remain(p)? {
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_MARKERS_REMAIN,
                "conflict markers remain in the file; edit it or use force",
            ));
        }
        if let Some(PendingOperation::Rebase { resolved, .. }) = &mut self.state.pending
            && !resolved.iter().any(|r| r.as_slice() == p.as_bytes())
        {
            resolved.push(p.as_bytes().to_vec());
            resolved.sort();
        }
        self.save_state()
    }

    fn markers_remain(&self, p: &RepoPath) -> Result<bool> {
        let Some(st) = self.safe.stat(p)? else {
            return Ok(false);
        };
        let content = match st.entry_mode() {
            Some(Mode::Symlink) => self.safe.read_link(p)?,
            Some(_) => self.safe.read(p)?,
            None => return Ok(false),
        };
        Ok(has_marker_lines(&content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_detection() {
        let rendered = scion_merge::render_markers(b"a\nb\n", b"a\nours\n", b"a\ntheirs\n");
        assert!(has_marker_lines(&rendered));
        assert!(has_marker_lines(b"x\r\n<<<<<<< ours\r\n"));
        assert!(has_marker_lines(b">>>>>>>"));
        assert!(!has_marker_lines(b"Title\n=======\n"));
        assert!(!has_marker_lines(b"<<<<<<<<< not a marker\n"));
        assert!(!has_marker_lines(b"a <<<<<<< ours\n"));
        assert!(!has_marker_lines(b""));
    }
}
