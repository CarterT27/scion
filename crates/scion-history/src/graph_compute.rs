//! SPEC 9.2 commit_graph columns (generation and skip pointers).

use scion_core::ObjectId;
use scion_store::ChangesetRow;

/// Compute `(generation, skip)` for a new changeset (SPEC 9.2).
///
/// - `generation = 1 + max(parent generations)`; a root changeset has generation 1.
/// - `skip[k]` is the first-parent ancestor at distance `2^k`, for every `k` while it exists.
///
/// `first_parent` is the row of `parents[0]` (`None` for a root changeset); `parents` holds the
/// rows of every parent. `skip_lookup(id, j)` must return `skip[j]` of changeset `id` (or `None`
/// when it has no such pointer); it is called for `k >= 2`.
pub fn compute_commit_graph(
    first_parent: Option<&ChangesetRow>,
    parents: &[ChangesetRow],
    skip_lookup: impl Fn(&ObjectId, usize) -> Option<ObjectId>,
) -> (u64, Vec<ObjectId>) {
    let generation = 1 + parents.iter().map(|p| p.generation).max().unwrap_or(0);
    let mut skip = Vec::new();
    if let Some(fp) = first_parent {
        // skip[0]: distance 1.
        skip.push(fp.id);
        // skip[k] = skip[k-1] of the node at distance 2^(k-1).
        let mut k = 1usize;
        loop {
            let prev = skip[k - 1];
            let next = if k == 1 {
                fp.skip.first().copied()
            } else {
                skip_lookup(&prev, k - 1)
            };
            match next {
                Some(id) => skip.push(id),
                None => break,
            }
            k += 1;
        }
    }
    (generation, skip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use scion_core::{ChangeId, Kind};
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn id(n: u8) -> ObjectId {
        ObjectId::new(Kind::Changeset, [n; 32])
    }

    fn row(id: ObjectId, generation: u64, skip: Vec<ObjectId>) -> ChangesetRow {
        ChangesetRow {
            id,
            encoded: Bytes::new(),
            generation,
            skip,
            change_id: ChangeId([0; 16]),
            created_by: "service:test".into(),
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    /// Build a row for `me` with the given parent ids, using `rows` as the store.
    fn add(
        rows: &mut HashMap<ObjectId, ChangesetRow>,
        me: ObjectId,
        parents: &[ObjectId],
    ) -> ChangesetRow {
        let parent_rows: Vec<ChangesetRow> = parents.iter().map(|p| rows[p].clone()).collect();
        let lookup = |c: &ObjectId, j: usize| rows.get(c).and_then(|r| r.skip.get(j).copied());
        let (g, skip) = compute_commit_graph(parent_rows.first(), &parent_rows, lookup);
        let r = row(me, g, skip);
        rows.insert(me, r.clone());
        r
    }

    #[test]
    fn root_has_generation_one() {
        let (g, skip) = compute_commit_graph(None, &[], |_, _| None);
        assert_eq!(g, 1);
        assert!(skip.is_empty());
    }

    #[test]
    fn ten_chain_skip_pointers() {
        let mut rows = HashMap::new();
        add(&mut rows, id(0), &[]);
        for n in 1..10u8 {
            add(&mut rows, id(n), &[id(n - 1)]);
        }
        // c9 is at generation 10; ancestors at distances 1, 2, 4, 8 are c8, c7, c5, c1.
        let c9 = &rows[&id(9)];
        assert_eq!(c9.generation, 10);
        assert_eq!(c9.skip, vec![id(8), id(7), id(5), id(1)]);
        // c8: distances 1, 2, 4, 8 -> c7, c6, c4, c0.
        assert_eq!(rows[&id(8)].skip, vec![id(7), id(6), id(4), id(0)]);
        // c1: only its parent.
        assert_eq!(rows[&id(1)].skip, vec![id(0)]);
        assert_eq!(rows[&id(0)].skip, Vec::<ObjectId>::new());
        // Every pointer really is the first-parent ancestor at distance 2^k.
        for n in 0..10usize {
            for (k, s) in rows[&id(n as u8)].skip.iter().enumerate() {
                assert_eq!(*s, id((n - (1usize << k)) as u8));
            }
        }
    }

    #[test]
    fn merge_generation_is_one_plus_max() {
        let mut rows = HashMap::new();
        add(&mut rows, id(0), &[]);
        add(&mut rows, id(1), &[id(0)]);
        add(&mut rows, id(2), &[id(1)]);
        add(&mut rows, id(3), &[id(2)]); // generation 4
        add(&mut rows, id(10), &[id(0)]); // generation 2
        // First parent is the short side; generation still follows the longest.
        let m = add(&mut rows, id(20), &[id(10), id(3)]);
        assert_eq!(m.generation, 5);
        // Skip pointers follow first parents only: c10 (1), c0 (2).
        assert_eq!(m.skip, vec![id(10), id(0)]);
        let m2 = add(&mut rows, id(21), &[id(3), id(10)]);
        assert_eq!(m2.generation, 5);
        assert_eq!(m2.skip, vec![id(3), id(2), id(0)]);
    }
}
