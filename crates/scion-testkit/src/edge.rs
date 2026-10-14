//! Hand-built edge-case repositories (SPEC 19.0 Corpus; P09).
//!
//! Every repository is a valid history under SPEC 4.4 path rules: names are 1-255 bytes with no
//! NUL or `/`, never `.`, `..` or `.scion`, and paths are at most 4096 bytes. No snapshot holds
//! a path that is both a file and a directory. Each history is linear unless its name says
//! otherwise, and contents are fixed, so the catalog is identical on every call.

use crate::corpus::{GenCommit, GenRepo};
use scion_refmodel::{MFile, MTree};

/// The SPEC 4.4 maximum path length in bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// The SPEC 4.4 maximum name length in bytes.
pub const MAX_NAME_BYTES: usize = 255;

fn file(content: &[u8]) -> MFile {
    MFile {
        mode: 1,
        content: content.to_vec(),
    }
}

fn exec(content: &[u8]) -> MFile {
    MFile {
        mode: 2,
        content: content.to_vec(),
    }
}

fn link(target: &[u8]) -> MFile {
    MFile {
        mode: 3,
        content: target.to_vec(),
    }
}

fn tree(entries: Vec<(Vec<u8>, MFile)>) -> MTree {
    entries.into_iter().collect()
}

fn p(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// A linear history of `snapshots` (commit `i` has parent `i - 1`).
fn linear(name: &str, snapshots: Vec<MTree>) -> GenRepo {
    let commits = snapshots
        .into_iter()
        .enumerate()
        .map(|(i, tree)| GenCommit {
            parents: if i == 0 { Vec::new() } else { vec![i - 1] },
            tree,
            author: b"Edge Case <edge@example.com>".to_vec(),
            message: format!("{name}: step {i}\n").into_bytes(),
            change_id: change_id(name, i),
        })
        .collect();
    GenRepo { commits }
}

/// Deterministic change id from the case name and commit index.
fn change_id(name: &str, i: usize) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(b"scion-testkit edge change id\0");
    h.update(name.as_bytes());
    h.update(&(i as u64).to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

/// A path of exactly [`MAX_PATH_BYTES`] bytes: 20 directories of 200 bytes and a 76-byte file
/// name (20 x 200 + 20 separators + 76 = 4096).
pub(crate) fn path_4096() -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_PATH_BYTES);
    for i in 0..20u8 {
        let mut name = vec![b'a' + (i % 26); 200];
        name[0] = b'd';
        out.extend_from_slice(&name);
        out.push(b'/');
    }
    out.extend(std::iter::repeat_n(b'f', MAX_PATH_BYTES - out.len()));
    out
}

/// A deeply nested path (one-byte directory names) of exactly [`MAX_PATH_BYTES`] bytes:
/// 2047 directories `d/` and the file name `ff`.
pub(crate) fn nested_4096() -> Vec<u8> {
    let mut out = Vec::with_capacity(MAX_PATH_BYTES);
    for _ in 0..2047 {
        out.extend_from_slice(b"d/");
    }
    out.extend_from_slice(b"ff");
    out
}

/// A name of exactly [`MAX_NAME_BYTES`] bytes starting with `prefix` (ASCII), padded with
/// `fill`.
fn name_255(prefix: &[u8], fill: u8) -> Vec<u8> {
    let mut n = prefix.to_vec();
    n.resize(MAX_NAME_BYTES, fill);
    n
}

/// Named edge cases (SPEC 19.0 Corpus, 4.4): empty root, 4096-byte path, 255-byte names,
/// case-only names, file/dir flips, CRLF, NUL content, exec flips, symlink retargets,
/// delete-and-readd, unterminated last lines.
///
/// Names (stable): `empty_root`, `deep_path_4096`, `deep_nesting_4096`, `long_names_255`,
/// `case_only_names`, `file_dir_flip`, `crlf`, `nul_content`, `exec_flip`,
/// `symlink_retarget`, `delete_and_readd`, `unterminated_last_line`, `empty_file`,
/// `merge_first_parent`.
pub fn edge_cases() -> Vec<(&'static str, GenRepo)> {
    let mut out: Vec<(&'static str, GenRepo)> = Vec::new();

    // Empty root, then content, then empty again.
    out.push((
        "empty_root",
        linear(
            "empty_root",
            vec![
                MTree::new(),
                tree(vec![(p("a.txt"), file(b"a\n"))]),
                MTree::new(),
            ],
        ),
    ));

    // A path at the 4096-byte limit (long names), edited, then removed.
    let deep = path_4096();
    out.push((
        "deep_path_4096",
        linear(
            "deep_path_4096",
            vec![
                tree(vec![(deep.clone(), file(b"deep\n"))]),
                tree(vec![
                    (deep.clone(), file(b"deep edited\n")),
                    (p("top.txt"), file(b"top\n")),
                ]),
                tree(vec![(p("top.txt"), file(b"top\n"))]),
            ],
        ),
    ));

    // A path at the 4096-byte limit made of 2048 components.
    let nested = nested_4096();
    out.push((
        "deep_nesting_4096",
        linear(
            "deep_nesting_4096",
            vec![
                tree(vec![(nested.clone(), file(b"nested\n"))]),
                tree(vec![(nested.clone(), file(b"nested edited\n"))]),
            ],
        ),
    ));

    // 255-byte names: ASCII, multi-byte UTF-8 and non-UTF-8 bytes, as file and directory.
    let ascii = name_255(b"n", b'x');
    let mut utf8 = Vec::new();
    while utf8.len() + "é".len() <= MAX_NAME_BYTES {
        utf8.extend_from_slice("é".as_bytes());
    }
    utf8.resize(MAX_NAME_BYTES, b'e');
    let raw = name_255(b"r", 0xff);
    let mut dir_file = name_255(b"D", b'y');
    dir_file.push(b'/');
    dir_file.extend_from_slice(&name_255(b"F", b'z'));
    out.push((
        "long_names_255",
        linear(
            "long_names_255",
            vec![
                tree(vec![
                    (ascii.clone(), file(b"ascii\n")),
                    (utf8.clone(), file(b"utf8\n")),
                    (raw.clone(), file(b"raw\n")),
                    (dir_file.clone(), file(b"dir\n")),
                ]),
                tree(vec![
                    (ascii.clone(), file(b"ascii 2\n")),
                    (utf8.clone(), file(b"utf8\n")),
                    (raw.clone(), exec(b"raw\n")),
                    (dir_file.clone(), file(b"dir 2\n")),
                ]),
            ],
        ),
    ));

    // Case-only differing names (distinct paths in SPEC 4.4).
    out.push((
        "case_only_names",
        linear(
            "case_only_names",
            vec![
                tree(vec![
                    (p("README"), file(b"upper\n")),
                    (p("readme"), file(b"lower\n")),
                    (p("ReadMe"), file(b"mixed\n")),
                    (p("Dir/x"), file(b"upper dir\n")),
                    (p("dir/x"), file(b"lower dir\n")),
                ]),
                tree(vec![
                    (p("README"), file(b"upper\n")),
                    (p("readme"), file(b"lower edited\n")),
                    (p("Dir/x"), file(b"upper dir\n")),
                    (p("dir/x"), file(b"lower dir\n")),
                    (p("dir/X"), file(b"upper x in lower dir\n")),
                ]),
            ],
        ),
    ));

    // A file becomes a directory and back.
    out.push((
        "file_dir_flip",
        linear(
            "file_dir_flip",
            vec![
                tree(vec![(p("a"), file(b"a file\n")), (p("z"), file(b"z\n"))]),
                tree(vec![
                    (p("a/b"), file(b"now a dir\n")),
                    (p("a/c/d"), file(b"nested\n")),
                    (p("z"), file(b"z\n")),
                ]),
                tree(vec![
                    (p("a"), file(b"a file again\n")),
                    (p("z"), file(b"z\n")),
                ]),
                tree(vec![
                    (p("a"), file(b"a file again\n")),
                    (p("z/y"), file(b"z dir\n")),
                ]),
            ],
        ),
    ));

    // CRLF and mixed line endings with line edits.
    out.push((
        "crlf",
        linear(
            "crlf",
            vec![
                tree(vec![
                    (p("crlf.txt"), file(b"one\r\ntwo\r\nthree\r\n")),
                    (p("mixed.txt"), file(b"lf\ncrlf\r\nlf\ncr only\rend\n")),
                ]),
                tree(vec![
                    (p("crlf.txt"), file(b"one\r\ntwo edited\r\nthree\r\n")),
                    (p("mixed.txt"), file(b"lf\ncrlf\nlf\ncr only\rend\r\n")),
                ]),
            ],
        ),
    ));

    // NUL bytes make content binary (SPEC 8.3 binary heuristic).
    out.push((
        "nul_content",
        linear(
            "nul_content",
            vec![
                tree(vec![
                    (p("bin/data.bin"), file(b"\x00\x01\x02binary\x00tail")),
                    (p("bin/late_nul.txt"), file(b"text text text\n\x00")),
                ]),
                tree(vec![
                    (p("bin/data.bin"), file(b"\x00\x01\x03binary\x00tail")),
                    (p("bin/late_nul.txt"), file(b"text text text\n\x00")),
                ]),
                tree(vec![
                    (p("bin/data.bin"), file(b"now text\n")),
                    (p("bin/late_nul.txt"), file(b"text text text\n\x00")),
                ]),
            ],
        ),
    ));

    // Executable flips with identical content (same blob, different mode).
    out.push((
        "exec_flip",
        linear(
            "exec_flip",
            vec![
                tree(vec![(p("run.sh"), file(b"#!/bin/sh\necho hi\n"))]),
                tree(vec![(p("run.sh"), exec(b"#!/bin/sh\necho hi\n"))]),
                tree(vec![(p("run.sh"), file(b"#!/bin/sh\necho hi\n"))]),
            ],
        ),
    ));

    // Symlink retargets and a symlink/file flip with identical bytes.
    out.push((
        "symlink_retarget",
        linear(
            "symlink_retarget",
            vec![
                tree(vec![
                    (p("a.txt"), file(b"a\n")),
                    (p("b.txt"), file(b"b\n")),
                    (p("link"), link(b"a.txt")),
                ]),
                tree(vec![
                    (p("a.txt"), file(b"a\n")),
                    (p("b.txt"), file(b"b\n")),
                    (p("link"), link(b"b.txt")),
                ]),
                tree(vec![
                    (p("a.txt"), file(b"a\n")),
                    (p("b.txt"), file(b"b\n")),
                    (p("link"), file(b"b.txt")),
                ]),
                tree(vec![
                    (p("a.txt"), file(b"a\n")),
                    (p("b.txt"), file(b"b\n")),
                    (p("link"), link(b"../outside/target")),
                ]),
            ],
        ),
    ));

    // Delete and re-add with the same and with different content.
    out.push((
        "delete_and_readd",
        linear(
            "delete_and_readd",
            vec![
                tree(vec![
                    (p("keep"), file(b"k\n")),
                    (p("x.txt"), file(b"same\n")),
                ]),
                tree(vec![(p("keep"), file(b"k\n"))]),
                tree(vec![
                    (p("keep"), file(b"k\n")),
                    (p("x.txt"), file(b"same\n")),
                ]),
                tree(vec![(p("keep"), file(b"k\n"))]),
                tree(vec![
                    (p("keep"), file(b"k\n")),
                    (p("x.txt"), file(b"different\n")),
                ]),
            ],
        ),
    ));

    // Unterminated last lines: add a newline, edit, remove it again.
    out.push((
        "unterminated_last_line",
        linear(
            "unterminated_last_line",
            vec![
                tree(vec![(p("u.txt"), file(b"one\ntwo\nthree"))]),
                tree(vec![(p("u.txt"), file(b"one\ntwo\nthree\n"))]),
                tree(vec![(p("u.txt"), file(b"one\nTWO\nthree"))]),
                tree(vec![(p("u.txt"), file(b"no newline at all"))]),
            ],
        ),
    ));

    // Zero-length content.
    out.push((
        "empty_file",
        linear(
            "empty_file",
            vec![
                tree(vec![(p("empty"), file(b""))]),
                tree(vec![(p("empty"), file(b"now\n"))]),
                tree(vec![(p("empty"), file(b""))]),
            ],
        ),
    ));

    // A merge whose snapshot keeps the first parent's version of a both-changed file.
    let base = tree(vec![(p("m.txt"), file(b"one\ntwo\nthree\n"))]);
    let ours = tree(vec![(p("m.txt"), file(b"one\nours\nthree\n"))]);
    let theirs = tree(vec![
        (p("m.txt"), file(b"one\ntheirs\nthree\n")),
        (p("t.txt"), file(b"theirs only\n")),
    ]);
    let merged = tree(vec![
        (p("m.txt"), file(b"one\nours\nthree\n")),
        (p("t.txt"), file(b"theirs only\n")),
    ]);
    let mut mfp = linear("merge_first_parent", vec![base, ours]);
    mfp.commits.push(GenCommit {
        parents: vec![0],
        tree: theirs,
        author: b"Edge Case <edge@example.com>".to_vec(),
        message: b"merge_first_parent: theirs\n".to_vec(),
        change_id: change_id("merge_first_parent", 2),
    });
    mfp.commits.push(GenCommit {
        parents: vec![1, 2],
        tree: merged,
        author: b"Edge Case <edge@example.com>".to_vec(),
        message: b"merge_first_parent: merge keeping ours\n".to_vec(),
        change_id: change_id("merge_first_parent", 3),
    });
    out.push(("merge_first_parent", mfp));

    out
}
