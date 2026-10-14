//! Hand-built edge-case repositories (SPEC 19.0 Corpus; P09).

use crate::corpus::GenRepo;

/// Named edge cases: empty root, 4096-byte path, 255-byte names, case-only names, file/dir
/// flips, CRLF, NUL content, exec flips, symlink retargets, delete-and-readd, unterminated
/// last lines.
pub fn edge_cases() -> Vec<(&'static str, GenRepo)> {
    todo!("P09")
}
