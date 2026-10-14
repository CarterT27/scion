//! SPEC 8.3 decision table: one test per row handled by `classify` (rows 1-6 and the content
//! fallthrough), plus `merge_mode` and the binary row's detector (SPEC 19.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

use scion_core::{ConflictKind, EntryInfo, Mode, ObjectId};
use scion_merge::{EntryDecision, Merge3, classify, is_binary, merge_mode, merge3};

fn e(mode: Mode, content: &str) -> EntryInfo {
    EntryInfo {
        mode,
        id: ObjectId::hash_blob(content.as_bytes()),
        size: content.len() as u64,
    }
}

fn file(c: &str) -> EntryInfo {
    e(Mode::File, c)
}

fn take(x: Option<EntryInfo>) -> EntryDecision {
    EntryDecision::Take(x)
}

#[test]
fn row1_o_eq_b_takes_t() {
    let (b, t) = (file("base"), file("theirs"));
    assert_eq!(classify(Some(&b), Some(&b), Some(&t)), take(Some(t)));
    // t deleted: the deletion is taken.
    assert_eq!(classify(Some(&b), Some(&b), None), take(None));
    // b and o absent: t's addition is taken.
    assert_eq!(classify(None, None, Some(&t)), take(Some(t)));
    // All equal.
    assert_eq!(classify(Some(&b), Some(&b), Some(&b)), take(Some(b)));
    assert_eq!(classify(None, None, None), take(None));
}

#[test]
fn row2_t_eq_b_takes_o() {
    let (b, o) = (file("base"), file("ours"));
    assert_eq!(classify(Some(&b), Some(&o), Some(&b)), take(Some(o)));
    assert_eq!(classify(Some(&b), None, Some(&b)), take(None));
    assert_eq!(classify(None, Some(&o), None), take(Some(o)));
}

#[test]
fn row3_o_eq_t_takes_o() {
    let (b, x) = (file("base"), file("same change"));
    assert_eq!(classify(Some(&b), Some(&x), Some(&x)), take(Some(x)));
    // Both deleted.
    assert_eq!(classify(Some(&b), None, None), take(None));
    // Both added identically.
    assert_eq!(classify(None, Some(&x), Some(&x)), take(Some(x)));
}

#[test]
fn row4_add_add() {
    let (o, t) = (file("ours"), file("theirs"));
    assert_eq!(
        classify(None, Some(&o), Some(&t)),
        EntryDecision::Conflict(ConflictKind::AddAdd)
    );
    // add_add wins over symlink (row order).
    let s = e(Mode::Symlink, "target");
    assert_eq!(
        classify(None, Some(&s), Some(&t)),
        EntryDecision::Conflict(ConflictKind::AddAdd)
    );
}

#[test]
fn row5_modify_delete() {
    let (b, x) = (file("base"), file("modified"));
    assert_eq!(
        classify(Some(&b), None, Some(&x)),
        EntryDecision::Conflict(ConflictKind::ModifyDelete)
    );
    assert_eq!(
        classify(Some(&b), Some(&x), None),
        EntryDecision::Conflict(ConflictKind::ModifyDelete)
    );
    // modify_delete wins over symlink (row order).
    let s = e(Mode::Symlink, "target");
    assert_eq!(
        classify(Some(&b), None, Some(&s)),
        EntryDecision::Conflict(ConflictKind::ModifyDelete)
    );
}

#[test]
fn row6_symlink() {
    let b = file("base");
    let s = e(Mode::Symlink, "target");
    // One side becomes a symlink, the other modifies the file.
    assert_eq!(
        classify(Some(&b), Some(&s), Some(&file("other"))),
        EntryDecision::Conflict(ConflictKind::Symlink)
    );
    // Both sides retarget a symlink differently.
    let (s1, s2) = (e(Mode::Symlink, "one"), e(Mode::Symlink, "two"));
    assert_eq!(
        classify(Some(&s), Some(&s1), Some(&s2)),
        EntryDecision::Conflict(ConflictKind::Symlink)
    );
    // The base is a symlink and both sides replace it with different files.
    assert_eq!(
        classify(Some(&s), Some(&file("x")), Some(&file("y"))),
        EntryDecision::Conflict(ConflictKind::Symlink)
    );
}

#[test]
fn otherwise_needs_content() {
    let (b, o, t) = (file("base"), file("ours"), file("theirs"));
    assert_eq!(
        classify(Some(&b), Some(&o), Some(&t)),
        EntryDecision::NeedContent
    );
    // A mode change on one side and a content change on the other also needs content; the
    // mode is merged by merge_mode.
    let ox = e(Mode::Executable, "ours");
    assert_eq!(
        classify(Some(&b), Some(&ox), Some(&t)),
        EntryDecision::NeedContent
    );
    assert_eq!(merge_mode(b.mode, ox.mode, t.mode), Mode::Executable);
}

#[test]
fn equality_is_mode_and_id() {
    let b = file("base");
    // A pure mode change on ours: o != b.
    let ox = e(Mode::Executable, "base");
    assert_eq!(classify(Some(&b), Some(&ox), Some(&b)), take(Some(ox)));
    // The size is not part of equality.
    let odd = EntryInfo { size: 999, ..b };
    let t = file("theirs");
    assert_eq!(classify(Some(&b), Some(&odd), Some(&t)), take(Some(t)));
}

#[test]
fn merge_mode_first_three_rows() {
    use Mode::*;
    assert_eq!(merge_mode(File, File, Executable), Executable); // o == b
    assert_eq!(merge_mode(File, Executable, File), Executable); // t == b
    assert_eq!(merge_mode(Executable, File, File), File); // o == t
    assert_eq!(merge_mode(File, File, File), File);
}

#[test]
fn binary_row_detector_boundary() {
    // SPEC 19.4: 0x00 at byte 7999 is binary; at byte 8000 is text.
    let mut at_7999 = vec![b'x'; 8001];
    at_7999[7999] = 0;
    assert!(is_binary(&at_7999));
    let mut at_8000 = vec![b'x'; 8001];
    at_8000[8000] = 0;
    assert!(!is_binary(&at_8000));
    // Exactly 8000 bytes with the NUL last is binary; 7999 bytes without NUL is text.
    let mut exact = vec![b'x'; 8000];
    assert!(!is_binary(&exact));
    exact[7999] = 0;
    assert!(is_binary(&exact));
    assert!(is_binary(&[0]));
    assert!(!is_binary(b""));
    // The content merge itself is byte-oriented; text with a late NUL still merges.
    let base = [vec![b'a'; 8000], b"\n\0tail\n".to_vec()].concat();
    let ours = [b"head\n".to_vec(), base.clone()].concat();
    assert!(!is_binary(&base));
    assert_eq!(merge3(&base, &ours, &base), Merge3::Clean(ours.clone()));
}
