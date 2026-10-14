//! Per-path classification by the SPEC 8.3 table (rows 1-6).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::{ConflictKind, EntryInfo, Mode};

/// Decision for one path before any content is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryDecision {
    /// Take this entry (`None` = absent/deleted).
    Take(Option<EntryInfo>),
    /// A conflict decided without content.
    Conflict(ConflictKind),
    /// A content merge is needed.
    NeedContent,
}

/// SPEC 8.3 rows in order: `o == b` take t; `t == b` take o; `o == t` take o; add_add;
/// modify_delete; symlink; otherwise NeedContent. Equality compares `(mode, id)`.
pub fn classify(
    b: Option<&EntryInfo>,
    o: Option<&EntryInfo>,
    t: Option<&EntryInfo>,
) -> EntryDecision {
    todo!("P02")
}

/// The first three SPEC 8.3 rows applied to modes.
pub fn merge_mode(b: Mode, o: Mode, t: Mode) -> Mode {
    todo!("P02")
}
