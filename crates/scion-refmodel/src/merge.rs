//! SPEC 8.3 tree-level three-way merge over structural trees (with the SPEC 18.3 order:
//! structural conflicts are checked before the table).

use crate::diff;
use crate::model::{has_dir, has_file_at_ancestor};
use crate::{MConflict, MFile, MPath, MTree};
use std::collections::BTreeSet;

/// SPEC 8.3 binary heuristic: a `0x00` within the first 8000 bytes.
fn is_binary(c: &[u8]) -> bool {
    c.iter().take(8000).any(|&b| b == 0)
}

/// Resolution of one path.
enum Entry {
    Take(Option<MFile>),
    Conflict(&'static str),
}

/// The SPEC 8.3 table for one path whose sides are `b`, `o`, `t`.
fn merge_entry(
    b: Option<&MFile>,
    o: Option<&MFile>,
    t: Option<&MFile>,
    max_text: u64,
    redacted: &BTreeSet<Vec<u8>>,
) -> Entry {
    if o == b {
        return Entry::Take(t.cloned());
    }
    if t == b || o == t {
        return Entry::Take(o.cloned());
    }
    let (b, o, t) = match (b, o, t) {
        (None, Some(_), Some(_)) => return Entry::Conflict("add_add"),
        (Some(b), Some(o), Some(t)) => (b, o, t),
        // One of o, t is absent and the other differs from b.
        _ => return Entry::Conflict("modify_delete"),
    };
    if b.mode == 3 || o.mode == 3 || t.mode == 3 {
        return Entry::Conflict("symlink");
    }
    if [b, o, t].iter().any(|f| redacted.contains(&f.content)) {
        return Entry::Conflict("redacted");
    }
    if [b, o, t]
        .iter()
        .any(|f| is_binary(&f.content) || f.content.len() as u64 > max_text)
    {
        return Entry::Conflict("binary");
    }
    match diff::merge3(&b.content, &o.content, &t.content) {
        Ok(content) => {
            // Mode merged by the first three rows (modes here are 1 or 2).
            let mode = if o.mode == b.mode { t.mode } else { o.mode };
            Entry::Take(Some(MFile { mode, content }))
        }
        Err(_) => Entry::Conflict("content"),
    }
}

/// Merge `o` and `t` against `b`. `paths` restricts the examined paths (rebase: the delta
/// paths); `redacted` lists contents treated as redacted (SPEC 8.2, 8.3, 13.6, 18.3).
///
/// Without `paths`, every path where `o` or `t` differs from `b` is examined (the `Merge(a, b)`
/// form over both FileDiffs). The result is `o` with every examined path's resolution applied.
/// Per path, in the SPEC 18.3 order:
///
/// 1. When `t` changed the path (`t != b`): `file_directory` if `t` puts a file where `o` has a
///    directory or a file at an ancestor, or `t` deletes it while `o` has a directory there.
/// 2. The SPEC 8.3 table: `o == b` takes `t`; `t == b` or `o == t` takes `o`; `add_add`;
///    `modify_delete`; `symlink` (any of `b`, `o`, `t` a symlink); `redacted` (any needed
///    content in `redacted`; checked before `binary` because redacted bytes are unavailable);
///    `binary` (any side with a `0x00` in its first 8000 bytes or longer than `max_text`);
///    otherwise line-diff3-v1 content merge (`content` on conflict) with the mode merged by the
///    first three rows.
///
/// All conflicts are returned, ascending by path.
pub fn merge_trees(
    b: &MTree,
    o: &MTree,
    t: &MTree,
    paths: Option<&BTreeSet<MPath>>,
    max_text: u64,
    redacted: &BTreeSet<Vec<u8>>,
) -> Result<MTree, Vec<MConflict>> {
    let examined: BTreeSet<MPath> = match paths {
        Some(ps) => ps.clone(),
        None => b
            .keys()
            .chain(o.keys())
            .chain(t.keys())
            .filter(|p| o.get(*p) != b.get(*p) || t.get(*p) != b.get(*p))
            .cloned()
            .collect(),
    };
    let mut result = o.clone();
    let mut conflicts = Vec::new();
    for p in examined {
        let (bp, op, tp) = (b.get(&p), o.get(&p), t.get(&p));
        if tp != bp {
            let structural = match tp {
                Some(_) => has_dir(o, &p) || has_file_at_ancestor(o, &p),
                None => has_dir(o, &p),
            };
            if structural {
                conflicts.push(MConflict {
                    path: p,
                    kind: "file_directory",
                });
                continue;
            }
        }
        match merge_entry(bp, op, tp, max_text, redacted) {
            Entry::Take(Some(f)) => {
                result.insert(p, f);
            }
            Entry::Take(None) => {
                result.remove(&p);
            }
            Entry::Conflict(kind) => conflicts.push(MConflict { path: p, kind }),
        }
    }
    if conflicts.is_empty() {
        Ok(result)
    } else {
        Err(conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_heuristic_boundary() {
        let mut c = vec![b'a'; 9000];
        c[7999] = 0;
        assert!(is_binary(&c));
        c[7999] = b'a';
        c[8000] = 0;
        assert!(!is_binary(&c));
    }
}
