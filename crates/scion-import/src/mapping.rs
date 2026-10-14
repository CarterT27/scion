//! Git-to-Scion mappings (SPEC 14; PLAN 6.4; P29).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::{ChangeId, Mode, Result};

/// `BLAKE3("scion-import-v1" || git commit id)[0..16]`.
pub fn change_id_for(git_commit: &[u8; 20]) -> ChangeId {
    todo!("P29")
}

/// 100644/100664 -> File, 100755 -> Executable, 120000 -> Symlink, 040000 -> `None` (tree);
/// 160000 (gitlink) and anything else -> error.
pub fn map_mode(git_mode: u32) -> Result<Option<Mode>> {
    todo!("P29")
}
