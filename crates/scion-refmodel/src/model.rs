//! Delta application and FileDiff over structural trees (SPEC 4.3.4, 4.4, 8.1, 18.2).
//!
//! `apply_delta` works on the flat tree: it validates the delta in the order the SPEC 18.2
//! reference walk would detect failures (directories deepest first; within a directory deletes,
//! then rebuilt child directories, then puts), then returns `T1 - deletes + puts`.

#[cfg(test)]
use crate::MFile;
use crate::{MDelta, MPath, MTree};
use std::collections::{BTreeMap, BTreeSet};

/// The directory containing `p` (empty for a top-level name).
pub(crate) fn parent_dir(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => &p[..i],
        None => b"",
    }
}

/// Is `dir` a proper ancestor directory of `p` (the root `""` is an ancestor of every non-empty
/// path)?
pub(crate) fn is_under(dir: &[u8], p: &[u8]) -> bool {
    if dir.is_empty() {
        return !p.is_empty();
    }
    p.len() > dir.len() && p.starts_with(dir) && p[dir.len()] == b'/'
}

/// Does `t` have a directory at the non-empty path `p` (some file strictly below it)?
pub(crate) fn has_dir(t: &MTree, p: &[u8]) -> bool {
    !p.is_empty() && t.keys().any(|k| is_under(p, k))
}

/// The proper ancestor directories of `p`, root `""` first.
pub(crate) fn ancestors(p: &[u8]) -> Vec<MPath> {
    let mut out = vec![Vec::new()];
    for (i, &c) in p.iter().enumerate() {
        if c == b'/' {
            out.push(p[..i].to_vec());
        }
    }
    out
}

/// Does `t` have a file at a proper ancestor of `p`?
pub(crate) fn has_file_at_ancestor(t: &MTree, p: &[u8]) -> bool {
    ancestors(p)
        .iter()
        .any(|a| !a.is_empty() && t.contains_key(a))
}

/// Number of path components (root = 0).
fn depth(p: &[u8]) -> usize {
    if p.is_empty() {
        0
    } else {
        p.iter().filter(|&&c| c == b'/').count() + 1
    }
}

/// SPEC 4.4 name rule.
fn valid_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.contains(&0)
        && !n.contains(&b'/')
        && n != b"."
        && n != b".."
        && !n.eq_ignore_ascii_case(b".scion")
}

/// SPEC 4.4 path rule for a non-root path (a delta path is never the root).
pub(crate) fn valid_path(p: &[u8]) -> bool {
    !p.is_empty() && p.len() <= 4096 && p.split(|&c| c == b'/').all(valid_name)
}

fn delta_path(d: &MDelta) -> &[u8] {
    match d {
        MDelta::Put(p, _) => p,
        MDelta::Delete(p) => p,
    }
}

/// Apply a delta; invalid deltas return the SPEC 8.1 subcode (`delete_absent`,
/// `delete_not_file`, `noop_put`, `file_directory`).
///
/// Shape failures are reported first, as SPEC 8.1 step 2 does: `delta_unsorted`,
/// `delta_duplicate`, `invalid_path`, and `invalid_mode` for a `Put` mode outside 1..=3. Then
/// the SPEC 18.2 order applies: touched directories deepest first (ties ascending by path);
/// in each directory its deletes (`delete_absent` when the path is absent from `t`,
/// `delete_not_file` when it is a directory), then its rebuilt child directories
/// (`file_directory` when `t` has a file there that the delta does not delete), then its puts
/// (`file_directory` when the path is still a directory, `noop_put` when the entry equals `t`).
pub fn apply_delta(t: &MTree, d: &[MDelta]) -> Result<MTree, &'static str> {
    for w in d.windows(2) {
        let (a, b) = (delta_path(&w[0]), delta_path(&w[1]));
        if a == b {
            return Err("delta_duplicate");
        }
        if a > b {
            return Err("delta_unsorted");
        }
    }
    for e in d {
        if !valid_path(delta_path(e)) {
            return Err("invalid_path");
        }
        if let MDelta::Put(_, f) = e
            && !(1..=3).contains(&f.mode)
        {
            return Err("invalid_mode");
        }
    }

    // The flat result, assuming validation passes.
    let mut result = t.clone();
    let mut deleted: BTreeSet<&[u8]> = BTreeSet::new();
    for e in d {
        match e {
            MDelta::Put(p, f) => {
                result.insert(p.clone(), f.clone());
            }
            MDelta::Delete(p) => {
                result.remove(p);
                deleted.insert(p);
            }
        }
    }

    let mut touched: BTreeSet<MPath> = BTreeSet::new();
    for e in d {
        touched.extend(ancestors(delta_path(e)));
    }
    let mut order: Vec<&MPath> = touched.iter().collect();
    order.sort_by(|a, b| depth(b).cmp(&depth(a)).then(a.cmp(b)));
    // dir -> rebuilt tree is non-empty
    let mut built_nonempty: BTreeMap<&[u8], bool> = BTreeMap::new();
    for dir in order {
        let dir: &[u8] = dir;
        // 1. deletes
        for e in d {
            if let MDelta::Delete(p) = e
                && parent_dir(p) == dir
            {
                if t.contains_key(p) {
                    continue;
                }
                return Err(if has_dir(t, p) {
                    "delete_not_file"
                } else {
                    "delete_absent"
                });
            }
        }
        // 2. rebuilt child directories
        for c in &touched {
            if !c.is_empty()
                && parent_dir(c) == dir
                && t.contains_key(c)
                && !deleted.contains(&c[..])
            {
                return Err("file_directory");
            }
        }
        // 3. puts
        for e in d {
            if let MDelta::Put(p, f) = e {
                if parent_dir(p) != dir {
                    continue;
                }
                let still_dir = match built_nonempty.get(&p[..]) {
                    Some(nonempty) => *nonempty,
                    None => has_dir(t, p),
                };
                if still_dir {
                    return Err("file_directory");
                }
                if t.get(p) == Some(f) {
                    return Err("noop_put");
                }
            }
        }
        built_nonempty.insert(dir, result.keys().any(|k| is_under(dir, k)));
    }
    Ok(result)
}

/// SPEC 4.3.4 FileDiff, ascending by path: `Put` for paths whose `(mode, content)` differs and
/// that are present in `b`, `Delete` for paths present only in `a`.
pub fn file_diff(a: &MTree, b: &MTree) -> Vec<MDelta> {
    let paths: BTreeSet<&MPath> = a.keys().chain(b.keys()).collect();
    let mut out = Vec::new();
    for p in paths {
        match (a.get(p), b.get(p)) {
            (_, Some(fb)) if a.get(p) != Some(fb) => out.push(MDelta::Put(p.clone(), fb.clone())),
            (Some(_), None) => out.push(MDelta::Delete(p.clone())),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(mode: u8, content: &[u8]) -> MFile {
        MFile {
            mode,
            content: content.to_vec(),
        }
    }

    fn tree_valid(t: &MTree) -> bool {
        t.keys()
            .all(|p| valid_path(p) && !has_file_at_ancestor(t, p))
    }

    fn tree(entries: &[(&str, u8, &str)]) -> MTree {
        entries
            .iter()
            .map(|(p, m, c)| (p.as_bytes().to_vec(), file(*m, c.as_bytes())))
            .collect()
    }

    #[test]
    fn helpers() {
        assert_eq!(parent_dir(b"a/b/c"), b"a/b");
        assert_eq!(parent_dir(b"a"), b"");
        assert!(is_under(b"a", b"a/b"));
        assert!(!is_under(b"a", b"ab/c"));
        assert!(!is_under(b"a", b"a"));
        assert!(is_under(b"", b"a"));
        assert_eq!(
            ancestors(b"a/b/c"),
            vec![b"".to_vec(), b"a".to_vec(), b"a/b".to_vec()]
        );
        let t = tree(&[("a/b", 1, "x")]);
        assert!(has_dir(&t, b"a"));
        assert!(!has_dir(&t, b"a/b"));
        assert!(has_file_at_ancestor(&t, b"a/b/c"));
    }

    #[test]
    fn path_rules() {
        assert!(valid_path(b"a/b"));
        for bad in [
            &b""[..],
            b"/a",
            b"a/",
            b"a//b",
            b".",
            b"a/..",
            b"a/\0",
            b".SCION",
            b"x/.Scion/y",
        ] {
            assert!(!valid_path(bad), "{bad:?}");
        }
        assert!(valid_path(&[b'n'; 255]));
        assert!(!valid_path(&[b'n'; 256]));
        let mut long = Vec::new();
        while long.len() < 4096 {
            if !long.is_empty() {
                long.push(b'/');
            }
            long.push(b'a');
        }
        long.truncate(4096);
        if long.last() == Some(&b'/') {
            long.pop();
        }
        assert!(valid_path(&long));
        let mut too_long = long.clone();
        too_long.extend_from_slice(b"/bb");
        assert!(too_long.len() > 4096);
        assert!(!valid_path(&too_long));
    }

    #[test]
    fn tree_validity() {
        assert!(tree_valid(&tree(&[("a/b", 1, ""), ("ab", 1, "")])));
        assert!(!tree_valid(&tree(&[("a", 1, ""), ("a/b", 1, "")])));
    }
}
