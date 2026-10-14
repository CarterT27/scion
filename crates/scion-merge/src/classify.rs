//! Per-path classification by the SPEC 8.3 table (rows 1-6).

use scion_core::{ConflictKind, EntryInfo, Mode};

/// Decision for one path before any content is read.
///
/// SPEC 8.3 three-way merge rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryDecision {
    /// Take this entry (`None` = absent/deleted).
    Take(Option<EntryInfo>),
    /// A conflict decided without content.
    Conflict(ConflictKind),
    /// A content merge is needed.
    NeedContent,
}

/// Entry equality as SPEC 8.3 defines it: absent, or `(mode, id)` (the size is not compared).
fn same(x: Option<&EntryInfo>, y: Option<&EntryInfo>) -> bool {
    match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.mode == y.mode && x.id == y.id,
        _ => false,
    }
}

/// SPEC 8.3 rows in order: `o == b` take t; `t == b` take o; `o == t` take o; add_add;
/// modify_delete; symlink; otherwise NeedContent. Equality compares `(mode, id)`.
///
/// Row 6 ("any side is a symlink") considers all three of `b`, `o` and `t`. The binary,
/// redacted and content rows need content and are decided by the caller after `NeedContent`.
pub fn classify(
    b: Option<&EntryInfo>,
    o: Option<&EntryInfo>,
    t: Option<&EntryInfo>,
) -> EntryDecision {
    // Row 1: o == b -> take t.
    if same(o, b) {
        return EntryDecision::Take(t.copied());
    }
    // Row 2: t == b -> take o.
    if same(t, b) {
        return EntryDecision::Take(o.copied());
    }
    // Row 3: o == t -> take o.
    if same(o, t) {
        return EntryDecision::Take(o.copied());
    }
    // From here on o, t and b are pairwise different.
    // Row 4: b absent, o and t present and different.
    if b.is_none() && o.is_some() && t.is_some() {
        return EntryDecision::Conflict(ConflictKind::AddAdd);
    }
    // Row 5: exactly one of o, t absent and the other differs from b.
    if o.is_none() != t.is_none() {
        return EntryDecision::Conflict(ConflictKind::ModifyDelete);
    }
    // Row 6: any side is a symlink (sides differ).
    if [b, o, t]
        .iter()
        .any(|e| e.is_some_and(|e| e.mode == Mode::Symlink))
    {
        return EntryDecision::Conflict(ConflictKind::Symlink);
    }
    EntryDecision::NeedContent
}

/// The first three SPEC 8.3 rows applied to modes.
///
/// `o == b` gives `t`; `t == b` gives `o`; `o == t` gives `o`. When all three differ (not
/// reachable for file modes after `classify`, since only File and Executable can be content
/// merged) the result is `o`.
pub fn merge_mode(b: Mode, o: Mode, t: Mode) -> Mode {
    if o == b { t } else { o }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_mode_rows() {
        use Mode::*;
        assert_eq!(merge_mode(File, File, Executable), Executable);
        assert_eq!(merge_mode(File, Executable, File), Executable);
        assert_eq!(merge_mode(File, Executable, Executable), Executable);
        assert_eq!(merge_mode(Executable, Executable, Executable), Executable);
    }
}
