//! Frozen `line-diff3-v1` golden vectors in `testdata/golden/diff/` (PLAN 3.4, SPEC 19.4).
//!
//! File format (one case per file, lowercase hex for byte strings):
//!
//! - `diff_*.json`: `{name, kind: "diff", description, a_hex, b_hex, edits, hunks}` where
//!   `edits` is the normalized script (`{"op": "equal", a, b, len}`, `{"op": "delete", a, len}`,
//!   `{"op": "insert", b, len}`) and `hunks` is `diff_lines` (`{base: [s, e], new: [s, e]}`).
//! - `merge_*.json`: `{name, kind: "merge", description, base_hex, ours_hex, theirs_hex,
//!   result_hex | conflicts, markers_hex}` where `conflicts` lists `{base, ours, theirs}` as
//!   `[start, end)` line ranges and `markers_hex` is `render_markers` (SPEC 11.7).
//!
//! Regenerate (only through an amendment) with `SCION_REGEN_GOLDEN=1 cargo test -p scion-merge
//! --test merge_golden`, then update `GOLDEN_DIGEST`.

mod merge_support;

use merge_support::*;
use scion_merge::{Edit, Merge3, diff_lines, merge3, myers, render_markers, split_lines};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// BLAKE3 over the sorted files: for each file, `name || 0x00 || len(u64 LE) || contents`.
const GOLDEN_DIGEST: &str = "d2167491839d2fc4cf59d4957f5d9555969018d1f987170311af3c88cf4c439b";

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum JEdit {
    Equal { a: usize, b: usize, len: usize },
    Delete { a: usize, len: usize },
    Insert { b: usize, len: usize },
}

impl From<&Edit> for JEdit {
    fn from(e: &Edit) -> Self {
        match *e {
            Edit::Equal { a, b, len } => JEdit::Equal { a, b, len },
            Edit::Delete { a, len } => JEdit::Delete { a, len },
            Edit::Insert { b, len } => JEdit::Insert { b, len },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
struct JHunk {
    base: [usize; 2],
    new: [usize; 2],
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
struct JConflict {
    base: [u32; 2],
    ours: [u32; 2],
    theirs: [u32; 2],
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
struct DiffCase {
    name: String,
    kind: String,
    description: String,
    a_hex: String,
    b_hex: String,
    edits: Vec<JEdit>,
    hunks: Vec<JHunk>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
struct MergeCase {
    name: String,
    kind: String,
    description: String,
    base_hex: String,
    ours_hex: String,
    theirs_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<JConflict>>,
    markers_hex: String,
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/diff")
}

fn diff_case(name: &str, description: &str, a: &[u8], b: &[u8]) -> DiffCase {
    let al = split_lines(a);
    let bl = split_lines(b);
    let edits = myers(&al, &bl);
    check_script(&al, &bl, &edits);
    DiffCase {
        name: name.to_string(),
        kind: "diff".to_string(),
        description: description.to_string(),
        a_hex: hex::encode(a),
        b_hex: hex::encode(b),
        edits: edits.iter().map(JEdit::from).collect(),
        hunks: diff_lines(&al, &bl)
            .into_iter()
            .map(|h| JHunk {
                base: [h.base.start, h.base.end],
                new: [h.new.start, h.new.end],
            })
            .collect(),
    }
}

fn merge_case(name: &str, description: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> MergeCase {
    let (result_hex, conflicts) = match merge3(base, ours, theirs) {
        Merge3::Clean(bytes) => (Some(hex::encode(bytes)), None),
        Merge3::Conflict(hunks) => (
            None,
            Some(
                hunks
                    .into_iter()
                    .map(|h| JConflict {
                        base: [h.base.start, h.base.end],
                        ours: [h.ours.start, h.ours.end],
                        theirs: [h.theirs.start, h.theirs.end],
                    })
                    .collect(),
            ),
        ),
    };
    MergeCase {
        name: name.to_string(),
        kind: "merge".to_string(),
        description: description.to_string(),
        base_hex: hex::encode(base),
        ours_hex: hex::encode(ours),
        theirs_hex: hex::encode(theirs),
        result_hex,
        conflicts,
        markers_hex: hex::encode(render_markers(base, ours, theirs)),
    }
}

fn s(x: &str) -> Vec<u8> {
    x.as_bytes().to_vec()
}

fn join(v: &[String]) -> Vec<u8> {
    v.concat().into_bytes()
}

/// Every golden case, built from constructors. Contract examples are additionally checked
/// against the literal expectations of `docs/contracts/line-diff3-v1.md` section 10.
fn constructors() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut put = |file: String, json: String| {
        assert!(files.insert(file, json).is_none(), "duplicate golden file");
    };

    // ---- diff: contract 10.2 -------------------------------------------------------------
    let mut n = 0usize;
    for ex in contract_diff_examples() {
        n += 1;
        let a = text(&ex.a);
        let b = text(&ex.b);
        let case = diff_case(
            &format!("contract_{}", ex.name),
            &format!("line-diff3-v1 10.2 {}: {}", ex.name, ex.description),
            &a,
            &b,
        );
        let want: Vec<JEdit> = ex.edits.iter().map(JEdit::from).collect();
        assert_eq!(case.edits, want, "{} disagrees with the contract", ex.name);
        put(
            format!("diff_{n:02}_contract_{}.json", ex.name.to_lowercase()),
            to_json(&case),
        );
    }

    // ---- diff: additional hand-written cases ---------------------------------------------
    let extra_diffs: Vec<(&str, &str, Vec<u8>, Vec<u8>)> = vec![
        (
            "identical",
            "identical inputs: one Equal run",
            s("a\nb\nc\n"),
            s("a\nb\nc\n"),
        ),
        (
            "append",
            "append a line at the end",
            s("a\nb\n"),
            s("a\nb\nc\n"),
        ),
        (
            "prepend",
            "prepend a line at the start",
            s("a\nb\n"),
            s("z\na\nb\n"),
        ),
        (
            "reverse3",
            "reverse three distinct lines (tie-breaks)",
            s("a\nb\nc\n"),
            s("c\nb\na\n"),
        ),
        (
            "reverse4",
            "reverse four distinct lines (tie-breaks)",
            s("a\nb\nc\nd\n"),
            s("d\nc\nb\na\n"),
        ),
        (
            "abcabc_acbacb",
            "interleaved transpositions",
            s("a\nb\nc\na\nb\nc\n"),
            s("a\nc\nb\na\nc\nb\n"),
        ),
        (
            "blank_insert",
            "insert a blank line among blank lines: goes at the end of the run",
            s("x\n\n\ny\n"),
            s("x\n\n\n\ny\n"),
        ),
        (
            "blank_delete",
            "delete a blank line among blank lines: the last copy goes",
            s("x\n\n\n\ny\n"),
            s("x\n\n\ny\n"),
        ),
        (
            "ababa_aba",
            "repeated pair: which copies are deleted",
            s("a\nb\na\nb\na\n"),
            s("a\nb\na\n"),
        ),
        (
            "aba_ababa",
            "repeated pair: which copies are inserted",
            s("a\nb\na\n"),
            s("a\nb\na\nb\na\n"),
        ),
        (
            "grow_replace",
            "replace two lines with three",
            s("p\nq\n"),
            s("x\ny\nz\n"),
        ),
        (
            "unterminated_both",
            "two unterminated single lines",
            s("x"),
            s("y"),
        ),
        ("newlines_only", "only empty lines", s("\n\n\n"), s("\n")),
        (
            "empty_to_crlf",
            "empty to CRLF content",
            s(""),
            s("a\r\nb\r\n"),
        ),
        (
            "cr_without_lf",
            "a trailing CR is part of an unterminated line",
            s("x\r"),
            s("x"),
        ),
        (
            "unterminated_kept",
            "delete a middle line, unterminated last line kept",
            s("a\nb\nc"),
            s("a\nc"),
        ),
        (
            "myers_reversed",
            "Myers paper example reversed: CBABAC -> ABCABBA",
            s("C\nB\nA\nB\nA\nC\n"),
            s("A\nB\nC\nA\nB\nB\nA\n"),
        ),
        (
            "brace_slider",
            "insert a block ending with a duplicated closing brace (slider ambiguity)",
            s("fn a() {\n}\n\nfn c() {\n}\n"),
            s("fn a() {\n}\n\nfn b() {\n}\n\nfn c() {\n}\n"),
        ),
        (
            "abab_baba_x3",
            "three rotated pairs",
            s("a\nb\na\nb\na\nb\n"),
            s("b\na\nb\na\nb\na\n"),
        ),
        (
            "delete_first_of_dupes",
            "delete the first line of a duplicated head",
            s("a\na\nb\n"),
            s("a\nb\n"),
        ),
        (
            "move_line_down",
            "move the first line to the end",
            s("1\n2\n3\n4\n"),
            s("2\n3\n4\n1\n"),
        ),
        (
            "move_line_up",
            "move the last line to the start",
            s("1\n2\n3\n4\n"),
            s("4\n1\n2\n3\n"),
        ),
        (
            "disjoint_long",
            "completely different texts of different lengths",
            s("a\nb\nc\nd\n"),
            s("w\nx\n"),
        ),
        (
            "terminator_gained_mid",
            "the same bytes with and without a final newline",
            s("a\nb\nc"),
            s("a\nb\nc\n"),
        ),
    ];
    for (slug, desc, a, b) in extra_diffs {
        n += 1;
        let case = diff_case(slug, desc, &a, &b);
        put(format!("diff_{n:02}_{slug}.json"), to_json(&case));
    }

    // ---- diff: seeded random tie-break-heavy cases --------------------------------------
    let mut rng = SplitMix(0x5C10_D1FF);
    let alphabet = ["a\n", "b\n", "c\n"];
    for j in 0..8 {
        let a = random_lines(&mut rng, 8, &alphabet);
        let b = random_edit(&mut rng, &a, &alphabet);
        let slug = format!("seeded_{j}");
        let case = diff_case(
            &slug,
            "seeded SplitMix64 small-alphabet case (tie-breaks)",
            &join(&a),
            &join(&b),
        );
        n += 1;
        put(format!("diff_{n:02}_{slug}.json"), to_json(&case));
    }

    // ---- merge: contract 10.3 ------------------------------------------------------------
    let mut n = 0usize;
    for ex in contract_merge_examples() {
        n += 1;
        let case = merge_case(
            &format!("contract_{}", ex.name),
            &format!("line-diff3-v1 10.3 {}: {}", ex.name, ex.description),
            &text(&ex.base),
            &text(&ex.ours),
            &text(&ex.theirs),
        );
        assert_eq!(
            merge3(&text(&ex.base), &text(&ex.ours), &text(&ex.theirs)),
            ex.result,
            "{} disagrees with the contract",
            ex.name
        );
        put(
            format!("merge_{n:02}_contract_{}.json", ex.name.to_lowercase()),
            to_json(&case),
        );
    }

    // ---- merge: additional hand-written cases --------------------------------------------
    let extra_merges: Vec<(&str, &str, Vec<u8>, Vec<u8>, Vec<u8>)> = vec![
        (
            "ours_is_base",
            "o == b: result is theirs",
            s("a\nb\nc\n"),
            s("a\nb\nc\n"),
            s("x\nb\ny\nz\n"),
        ),
        (
            "theirs_is_base",
            "t == b: result is ours",
            s("a\nb\nc\n"),
            s("a\nQ\n"),
            s("a\nb\nc\n"),
        ),
        (
            "ours_is_theirs",
            "o == t: result is ours",
            s("a\nb\nc\n"),
            s("c\nb\na\n"),
            s("c\nb\na\n"),
        ),
        (
            "both_delete_same",
            "both delete the same line: applied once",
            s("a\nb\nc\n"),
            s("a\nc\n"),
            s("a\nc\n"),
        ),
        (
            "delete_vs_modify",
            "one deletes, the other modifies the same line",
            s("a\nb\nc\n"),
            s("a\nc\n"),
            s("a\nB\nc\n"),
        ),
        (
            "insert_same_point_diff",
            "different insertions at the same point",
            s("a\nb\n"),
            s("a\nX\nb\n"),
            s("a\nY\nb\n"),
        ),
        (
            "insert_same_point_same",
            "identical insertions at the same point",
            s("a\nb\n"),
            s("a\nX\nb\n"),
            s("a\nX\nb\n"),
        ),
        (
            "insert_touches_change_start",
            "insertion at the start touches a change of line 0",
            s("a\nb\nc\n"),
            s("N\na\nb\nc\n"),
            s("A\nb\nc\n"),
        ),
        (
            "append_both_diff",
            "both append different lines at the end",
            s("a\n"),
            s("a\nx\n"),
            s("a\ny\n"),
        ),
        (
            "one_clean_one_conflict",
            "a clean region and a conflicting region",
            s("1\n2\n3\n4\n5\n6\n"),
            s("X\n2\n3\n4\nO\n6\n"),
            s("1\n2\n3\n4\nT\n6\n"),
        ),
        (
            "two_conflicts",
            "two conflicting regions",
            s("1\n2\n3\n4\n5\n"),
            s("A\n2\n3\n4\nC\n"),
            s("B\n2\n3\n4\nD\n"),
        ),
        (
            "adjacent_chain",
            "o, t, o change consecutive lines: one region",
            s("1\n2\n3\n4\n5\n"),
            s("1\nA\n3\nC\n5\n"),
            s("1\n2\nB\n4\n5\n"),
        ),
        (
            "identical_plus_disjoint",
            "an identical change plus disjoint one-sided changes",
            s("1\n2\n3\n4\n5\n6\n7\n"),
            s("O\n2\n3\nS\n5\n6\n7\n"),
            s("1\n2\n3\nS\n5\n6\nT\n"),
        ),
        (
            "unterminated_append",
            "ours appends after an unterminated last line, theirs edits line 0",
            s("a\nb"),
            s("a\nb\nc"),
            s("A\nb"),
        ),
        (
            "unterminated_both_terminate",
            "both terminate the final line",
            s("a\nb"),
            s("a\nb\n"),
            s("a\nb\n"),
        ),
        (
            "unterminated_conflict",
            "both change an unterminated final line differently",
            s("a\nb"),
            s("a\nB"),
            s("a\nb\n"),
        ),
        (
            "crlf_to_lf_vs_edit",
            "ours converts CRLF to LF, theirs edits a line",
            s("a\r\nb\r\nc\r\n"),
            s("a\nb\nc\n"),
            s("a\r\nB\r\nc\r\n"),
        ),
        (
            "crlf_disjoint",
            "disjoint edits in a CRLF file stay CRLF",
            s("a\r\nb\r\nc\r\nd\r\ne\r\n"),
            s("A\r\nb\r\nc\r\nd\r\ne\r\n"),
            s("a\r\nb\r\nc\r\nd\r\nE\r\n"),
        ),
        (
            "empty_base_one_side",
            "empty base, only ours adds",
            s(""),
            s("x\ny\n"),
            s(""),
        ),
        (
            "delete_all_one_side",
            "ours deletes everything, theirs unchanged",
            s("a\nb\n"),
            s(""),
            s("a\nb\n"),
        ),
        (
            "delete_all_vs_modify",
            "ours deletes everything, theirs modifies",
            s("a\nb\n"),
            s(""),
            s("a\nB\n"),
        ),
        (
            "swap_vs_append",
            "tie-break sensitive: AB->BA against an append",
            s("A\nB\n"),
            s("B\nA\n"),
            s("A\nB\nC\n"),
        ),
        (
            "dupes_delete_vs_append",
            "repeated lines: delete one copy against an append",
            s("a\na\na\n"),
            s("a\na\n"),
            s("a\na\na\nb\n"),
        ),
        (
            "dupes_insert_both",
            "repeated lines inserted on both sides",
            s("x\ny\n"),
            s("x\nx\ny\n"),
            s("x\ny\ny\n"),
        ),
        (
            "insert_inside_deletion",
            "an insertion inside a deleted range",
            s("a\nb\nc\nd\n"),
            s("a\nd\n"),
            s("a\nb\nN\nc\nd\n"),
        ),
        (
            "far_apart",
            "far apart edits on a longer file",
            s("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n"),
            s("1\nX\n3\n4\n5\n6\n7\n8\n9\n10\n"),
            s("1\n2\n3\n4\n5\n6\n7\n8\nY\n10\n"),
        ),
    ];
    for (slug, desc, b, o, t) in extra_merges {
        n += 1;
        let case = merge_case(slug, desc, &b, &o, &t);
        put(format!("merge_{n:02}_{slug}.json"), to_json(&case));
    }

    // ---- merge: seeded random cases ------------------------------------------------------
    let mut rng = SplitMix(0x3E26_E3A1);
    let alphabet = ["a\n", "b\n", "c\n", "d\n", "e"];
    for j in 0..8 {
        let b = random_lines(&mut rng, 8, &alphabet);
        let o = random_edit(&mut rng, &b, &alphabet);
        let t = random_edit(&mut rng, &b, &alphabet);
        let slug = format!("seeded_{j}");
        let case = merge_case(
            &slug,
            "seeded SplitMix64 small-alphabet merge",
            &join(&b),
            &join(&o),
            &join(&t),
        );
        n += 1;
        put(format!("merge_{n:02}_{slug}.json"), to_json(&case));
    }

    files
}

/// Pretty JSON with a trailing newline.
fn to_json<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("serialize golden case");
    s.push('\n');
    s
}

fn read_dir_files() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(golden_dir()) else {
        return out;
    };
    for entry in rd {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") {
            out.insert(
                name,
                std::fs::read_to_string(entry.path()).expect("read golden"),
            );
        }
    }
    out
}

fn digest(files: &BTreeMap<String, String>) -> String {
    let mut h = blake3::Hasher::new();
    for (name, content) in files {
        h.update(name.as_bytes());
        h.update(&[0]);
        h.update(&(content.len() as u64).to_le_bytes());
        h.update(content.as_bytes());
    }
    h.finalize().to_hex().to_string()
}

#[test]
fn golden_files_match_constructors() {
    let want = constructors();
    if std::env::var("SCION_REGEN_GOLDEN").as_deref() == Ok("1") {
        let dir = golden_dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        for name in read_dir_files().keys() {
            std::fs::remove_file(dir.join(name)).expect("remove stale golden");
        }
        for (name, content) in &want {
            std::fs::write(dir.join(name), content).expect("write golden");
        }
        eprintln!(
            "regenerated {} golden files; digest {}",
            want.len(),
            digest(&want)
        );
    }
    let have = read_dir_files();
    assert_eq!(
        have.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "golden file set differs from constructors"
    );
    for (name, content) in &want {
        assert_eq!(
            &have[name], content,
            "golden file {name} differs from its constructor"
        );
    }
}

#[test]
fn golden_digest_is_frozen() {
    let have = read_dir_files();
    assert!(
        have.len() >= 60,
        "at least 60 golden vectors, found {}",
        have.len()
    );
    assert_eq!(digest(&have), GOLDEN_DIGEST, "testdata/golden/diff changed");
}

#[test]
fn golden_vectors_reproduce() {
    let have = read_dir_files();
    let mut names = Vec::new();
    for (file, content) in &have {
        if file.starts_with("diff_") {
            let case: DiffCase = serde_json::from_str(content).expect(file);
            assert_eq!(case.kind, "diff");
            let a = hex::decode(&case.a_hex).expect("a_hex");
            let b = hex::decode(&case.b_hex).expect("b_hex");
            let al = split_lines(&a);
            let bl = split_lines(&b);
            let edits: Vec<JEdit> = myers(&al, &bl).iter().map(JEdit::from).collect();
            assert_eq!(edits, case.edits, "{file} edits");
            let hunks: Vec<JHunk> = diff_lines(&al, &bl)
                .into_iter()
                .map(|h| JHunk {
                    base: [h.base.start, h.base.end],
                    new: [h.new.start, h.new.end],
                })
                .collect();
            assert_eq!(hunks, case.hunks, "{file} hunks");
            names.push(case.name);
        } else if file.starts_with("merge_") {
            let case: MergeCase = serde_json::from_str(content).expect(file);
            assert_eq!(case.kind, "merge");
            let b = hex::decode(&case.base_hex).expect("base_hex");
            let o = hex::decode(&case.ours_hex).expect("ours_hex");
            let t = hex::decode(&case.theirs_hex).expect("theirs_hex");
            match (merge3(&b, &o, &t), &case.result_hex, &case.conflicts) {
                (Merge3::Clean(bytes), Some(want), None) => {
                    assert_eq!(hex::encode(bytes), *want, "{file} result")
                }
                (Merge3::Conflict(hunks), None, Some(want)) => {
                    let got: Vec<JConflict> = hunks
                        .into_iter()
                        .map(|h| JConflict {
                            base: [h.base.start, h.base.end],
                            ours: [h.ours.start, h.ours.end],
                            theirs: [h.theirs.start, h.theirs.end],
                        })
                        .collect();
                    assert_eq!(&got, want, "{file} conflicts");
                }
                (got, _, _) => panic!("{file}: outcome mismatch, got {got:?}"),
            }
            assert_eq!(
                hex::encode(render_markers(&b, &o, &t)),
                case.markers_hex,
                "{file} markers"
            );
            names.push(case.name);
        } else {
            panic!("unexpected golden file {file}");
        }
    }
    for ex in contract_diff_examples() {
        assert!(
            names.contains(&format!("contract_{}", ex.name)),
            "{} missing",
            ex.name
        );
    }
    for ex in contract_merge_examples() {
        assert!(
            names.contains(&format!("contract_{}", ex.name)),
            "{} missing",
            ex.name
        );
    }
    let diffs = have.keys().filter(|k| k.starts_with("diff_")).count();
    let merges = have.keys().filter(|k| k.starts_with("merge_")).count();
    assert!(diffs >= 30 && merges >= 30, "diff {diffs}, merge {merges}");
}
