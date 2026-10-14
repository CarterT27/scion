//! Sparse materialization model (SPEC 11.2, 11.3).

use crate::model::is_under;
use crate::{MPath, MTree};

/// The files of `t` a working copy with sparse prefixes `sparse` materializes (SPEC 11.2).
///
/// A file is in scope when a profile entry equals it or is an ancestor directory of it
/// (component-wise, so `a` covers `a/x` but not `ab`); `""` covers the whole tree. An empty
/// profile materializes nothing. Directories are implied by the files (the model is
/// file-level), so directories created on the way to a scoped prefix carry no files.
pub fn materialize(t: &MTree, sparse: &[MPath]) -> MTree {
    t.iter()
        .filter(|(p, _)| sparse.iter().any(|s| s == *p || is_under(s, p)))
        .map(|(p, f)| (p.clone(), f.clone()))
        .collect()
}
