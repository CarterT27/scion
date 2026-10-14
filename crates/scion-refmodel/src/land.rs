//! Sequential land model (SPEC 8.4, 18.4).

use crate::{MConflict, MDelta, MRebaseError, MRepo, MTree, apply_delta};

/// `limits.merge_max_text_bytes` default (SPEC 6.1, 6.5).
const MERGE_MAX_TEXT_BYTES: u64 = 10_485_760;

/// Apply each change's delta in order as a rebase onto the running result; the first failure
/// returns its index and conflicts (SPEC 8.4, 18.4).
///
/// Every change is a one-parent changeset on `base` whose tree is `apply_delta(base, delta)`.
/// The running result starts as the `base` changeset; each change is rebased onto it (SPEC 8.2,
/// with the default `merge_max_text_bytes`), so the first change takes the fast path. A failure
/// returns `(index, conflicts)`; the conflict list is empty when the delta is invalid against
/// `base` or the rebase is EMPTY_CHANGE (Land without `allow_empty` fails on it).
pub fn land_sequential(
    base: &MTree,
    changes: &[Vec<MDelta>],
) -> Result<MTree, (usize, Vec<MConflict>)> {
    let mut repo = MRepo::new();
    let root = repo.commit(&[], base.clone());
    let mut onto = root;
    for (i, d) in changes.iter().enumerate() {
        let tree = apply_delta(base, d).map_err(|_| (i, Vec::new()))?;
        let c = repo.commit(&[root], tree);
        onto = match repo.rebase(c, onto, MERGE_MAX_TEXT_BYTES) {
            Ok(r) => r,
            Err(MRebaseError::Conflict(cs)) => return Err((i, cs)),
            Err(MRebaseError::Empty) => return Err((i, Vec::new())),
        };
    }
    Ok(repo.tree(onto).clone())
}
