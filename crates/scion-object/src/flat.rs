//! Flat file-level trees and the pure SPEC 4.3.4 FileDiff.

use crate::changeset::{DeltaEntry, DeltaOp};
use crate::path::RepoPath;
use scion_core::EntryInfo;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// File-level entries only (modes 1-3), keyed by path.
pub type FlatTree = BTreeMap<RepoPath, EntryInfo>;

fn put(path: &RepoPath, e: &EntryInfo) -> DeltaEntry {
    DeltaEntry {
        path: path.clone(),
        op: DeltaOp::Put {
            mode: e.mode,
            id: e.id,
            size: e.size,
        },
    }
}

fn delete(path: &RepoPath) -> DeltaEntry {
    DeltaEntry {
        path: path.clone(),
        op: DeltaOp::Delete,
    }
}

/// SPEC 4.3.4 FileDiff: every path whose `(mode, id)` differs, Put when present in `b`,
/// Delete when absent; ascending by path.
///
/// A file replaced by a directory (or the reverse) yields a Delete of the file plus Puts of
/// the files below the directory. Size is carried from `b` but is not compared: it is a
/// function of the blob id.
pub fn file_diff(a: &FlatTree, b: &FlatTree) -> Vec<DeltaEntry> {
    let mut out = Vec::new();
    let mut ia = a.iter().peekable();
    let mut ib = b.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => break,
            (Some((pa, _)), None) => {
                out.push(delete(pa));
                ia.next();
            }
            (None, Some((pb, eb))) => {
                out.push(put(pb, eb));
                ib.next();
            }
            (Some((pa, ea)), Some((pb, eb))) => match pa.cmp(pb) {
                Ordering::Less => {
                    out.push(delete(pa));
                    ia.next();
                }
                Ordering::Greater => {
                    out.push(put(pb, eb));
                    ib.next();
                }
                Ordering::Equal => {
                    if ea.mode != eb.mode || ea.id != eb.id {
                        out.push(put(pb, eb));
                    }
                    ia.next();
                    ib.next();
                }
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{Mode, ObjectId};

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    fn info(content: &str, mode: Mode) -> EntryInfo {
        EntryInfo {
            mode,
            id: ObjectId::hash_blob(content.as_bytes()),
            size: content.len() as u64,
        }
    }

    fn flat(entries: &[(&str, &str, Mode)]) -> FlatTree {
        entries
            .iter()
            .map(|(path, c, m)| (p(path), info(c, *m)))
            .collect()
    }

    fn render(d: &[DeltaEntry]) -> Vec<String> {
        d.iter()
            .map(|e| {
                let op = match &e.op {
                    DeltaOp::Put { mode, .. } => format!("put{}", *mode as u8),
                    DeltaOp::Delete => "del".to_owned(),
                };
                format!("{op} {}", e.path)
            })
            .collect()
    }

    #[test]
    fn identical_is_empty() {
        let a = flat(&[("a", "1", Mode::File), ("b/c", "2", Mode::File)]);
        assert!(file_diff(&a, &a).is_empty());
        assert!(file_diff(&FlatTree::new(), &FlatTree::new()).is_empty());
    }

    #[test]
    fn add_modify_delete_mode_change() {
        let a = flat(&[
            ("a", "1", Mode::File),
            ("b", "2", Mode::File),
            ("c", "3", Mode::File),
        ]);
        let b = flat(&[
            ("a", "1", Mode::Executable),
            ("b", "22", Mode::File),
            ("d", "4", Mode::Symlink),
        ]);
        assert_eq!(
            render(&file_diff(&a, &b)),
            vec!["put2 a", "put1 b", "del c", "put3 d"]
        );
    }

    #[test]
    fn file_to_dir_and_back() {
        let a = flat(&[("x", "file", Mode::File), ("y", "1", Mode::File)]);
        let b = flat(&[
            ("x/one", "1", Mode::File),
            ("x/two", "2", Mode::File),
            ("y", "1", Mode::File),
        ]);
        assert_eq!(
            render(&file_diff(&a, &b)),
            vec!["del x", "put1 x/one", "put1 x/two"]
        );
        assert_eq!(
            render(&file_diff(&b, &a)),
            vec!["put1 x", "del x/one", "del x/two"]
        );
    }

    #[test]
    fn ordering_is_bytewise_path_order() {
        // "a.txt" (0x2e) sorts before "a/b" (0x2f), which sorts before "a0".
        let a = FlatTree::new();
        let b = flat(&[
            ("a0", "1", Mode::File),
            ("a/b", "2", Mode::File),
            ("a.txt", "3", Mode::File),
        ]);
        assert_eq!(
            render(&file_diff(&a, &b)),
            vec!["put1 a.txt", "put1 a/b", "put1 a0"]
        );
    }

    #[test]
    fn same_id_different_size_is_not_a_change() {
        let a = flat(&[("a", "1", Mode::File)]);
        let mut b = a.clone();
        if let Some(e) = b.get_mut(&p("a")) {
            e.size = 99;
        }
        assert!(file_diff(&a, &b).is_empty());
    }
}
