//! Pure merge engine `line-diff3-v1` (SPEC 8.3; PLAN 2.5).
//!
//! The behaviour is defined normatively by `docs/contracts/line-diff3-v1.md`; when this code
//! and that document disagree, the document wins.

mod binary;
mod classify;
mod hunks;
mod markers;
mod merge3;
mod myers;
mod origins;
mod split;
mod unified;

pub use binary::is_binary;
pub use classify::{EntryDecision, classify, merge_mode};
pub use hunks::{Hunk, diff_lines};
pub use markers::render_markers;
pub use merge3::{Merge3, merge3};
pub use myers::{Edit, myers};
pub use origins::line_origins;
pub use split::split_lines;
pub use unified::{TextHunk, unified_hunks};

/// The merge engine identifier (SPEC 8.3 `merge_engine`).
pub const ENGINE: &str = "line-diff3-v1";
