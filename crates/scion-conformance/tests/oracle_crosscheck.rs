//! Oracle cross-validation (P17): the independent reference model (`scion-refmodel`, SPEC
//! 19.0) against the production encoders and merge engine, so that a divergence surfaces here
//! before any engine depends on either side.
//!
//! 1. Every `testdata/golden/objects` vector is reproduced byte-for-byte (and id-for-id) by the
//!    refmodel encoder, built from the vector's description (SPEC 19.1, 4.1-4.3).
//! 2. Random valid trees, Metas and changesets: scion-object bytes and ids equal the refmodel's
//!    (SPEC 19.1 id agreement; 10^4 cases by default, `full_*` 10^6 with `SCION_FULL=1`).
//! 3. Every `testdata/golden/diff` vector is reproduced by `scion_refmodel::diff` (Myers edit
//!    runs, hunks, merge3 results and conflict ranges; `docs/contracts/line-diff3-v1.md`).
//! 4. Differential `scion_merge::merge3` vs `scion_refmodel::diff::merge3_full` on generated
//!    text triples (CRLF, unterminated last lines, identical edits), plus `myers`/`diff_lines`
//!    vs the refmodel (SPEC 19.4).
//! 5. `scion_object::file_diff` vs `scion_refmodel::file_diff` on random flat trees (SPEC 4.3.4).
//! 6. `scion_testkit::encode_tree` root ids and objects equal the refmodel's trees.
//!
//! A failing property panics with the proptest-minimized case. Production files and refmodel
//! files are never edited from here: disagreements are routed as amendments.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

mod oracle_support;

use oracle_support::*;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use scion_core::{ChangeId, Kind, ObjectId};
use scion_merge::{Edit, Hunk, Merge3};
use scion_object::{Canonical, Changeset, Meta, Tree};
use scion_refmodel::enc::{
    KIND_CHANGESET, KIND_META, KIND_TREE, blob_id, changeset_bytes, changeset_for, changeset_id,
    id_text, make_id, meta_bytes, meta_id, put_bytes, put_uvarint, tree_bytes_for_dir, tree_id,
};
use scion_refmodel::{MChangeset, MMeta, MTree};
use scion_testkit::{CorpusSpec, GenRepo, edge_cases, encode_tree, generate};
use std::collections::{BTreeMap, BTreeSet};

// =============================================================================================
// 1. Golden object vectors through the refmodel encoder (SPEC 19.1)
// =============================================================================================

/// A golden case rebuilt from its description with the refmodel encoder: canonical bytes
/// (raw content for blobs) and the raw id.
struct ModelCase {
    bytes: Vec<u8>,
    id: [u8; 34],
}

fn tree_case(t: &MTree) -> ModelCase {
    ModelCase {
        bytes: tree_bytes_for_dir(t, b""),
        id: tree_id(t),
    }
}

fn meta_case(m: &MMeta) -> ModelCase {
    ModelCase {
        bytes: meta_bytes(m),
        id: meta_id(m),
    }
}

fn changeset_case(c: &MChangeset) -> ModelCase {
    ModelCase {
        bytes: changeset_bytes(c),
        id: changeset_id(c),
    }
}

/// Replace the trailing empty extension list (a single `0x00`) of model bytes with the given
/// extensions, encoded with the refmodel's SPEC 4.1 primitives. The refmodel's tree and
/// changeset encoders always emit empty extensions, so even-extension vectors are checked this
/// way.
fn with_extensions(mut bytes: Vec<u8>, ext: &[(u64, &[u8])]) -> Vec<u8> {
    assert_eq!(
        bytes.pop(),
        Some(0),
        "model bytes end in an empty extension list"
    );
    put_uvarint(&mut bytes, ext.len() as u64);
    for (tag, value) in ext {
        put_uvarint(&mut bytes, *tag);
        put_bytes(&mut bytes, value);
    }
    bytes
}

/// SPEC 9.2 HistoryNode (derived kind 0x10), encoded with the refmodel primitives in field
/// order: format_version, kind u8, content id, changeset id, list<parent id>,
/// list<(name bytes, node id)>, extensions. The refmodel has no HistoryNode type, so this
/// test-local encoder is the oracle side.
fn history_node_case(
    kind: u8,
    content: [u8; 34],
    changeset: [u8; 34],
    parents: &[[u8; 34]],
    children: &[(&[u8], [u8; 34])],
) -> ModelCase {
    let mut b = Vec::new();
    put_uvarint(&mut b, 1);
    b.push(kind);
    b.extend_from_slice(&content);
    b.extend_from_slice(&changeset);
    put_uvarint(&mut b, parents.len() as u64);
    for p in parents {
        b.extend_from_slice(p);
    }
    put_uvarint(&mut b, children.len() as u64);
    for (name, id) in children {
        put_bytes(&mut b, name);
        b.extend_from_slice(id);
    }
    put_uvarint(&mut b, 0);
    ModelCase {
        id: make_id(0x10, &b),
        bytes: b,
    }
}

fn mtree(entries: &[(&[u8], u8, &[u8])]) -> MTree {
    entries
        .iter()
        .map(|(p, m, c)| (p.to_vec(), mfile(*m, c)))
        .collect()
}

fn ascending16(start: u8) -> [u8; 16] {
    std::array::from_fn(|i| start + i as u8)
}

/// Every golden object vector rebuilt from its description (testdata/golden/objects).
fn model_golden_objects() -> BTreeMap<&'static str, ModelCase> {
    let mut out = BTreeMap::new();
    let hello = b"hello\n".as_slice();

    out.insert(
        "blob_empty",
        ModelCase {
            bytes: Vec::new(),
            id: blob_id(b""),
        },
    );
    out.insert(
        "blob_hello",
        ModelCase {
            bytes: hello.to_vec(),
            id: blob_id(hello),
        },
    );

    let empty = MTree::new();
    let single = mtree(&[(b"hello.txt", 1, hello)]);
    let subdir = mtree(&[(b"inner.txt", 1, b"inner\n")]);
    let all_modes = mtree(&[
        (b"README", 1, hello),
        (b"bin/inner.txt", 1, b"inner\n"),
        (b"link", 3, b"../target"),
        (b"run.sh", 2, b"#!/bin/sh\necho hi\n"),
    ]);
    let x200 = vec![b'x'; 200];
    let bytewise = mtree(&[
        (b"B", 1, b"B"),
        (b"a", 1, b"a"),
        (&[0xc3, 0xa9], 1, &x200),
        (&[0xff], 2, b""),
    ]);
    out.insert("tree_empty", tree_case(&empty));
    out.insert("tree_single_file", tree_case(&single));
    out.insert("tree_subdir", tree_case(&subdir));
    out.insert("tree_all_modes", tree_case(&all_modes));
    out.insert("tree_bytewise_names", tree_case(&bytewise));
    {
        let bytes = with_extensions(tree_bytes_for_dir(&single, b""), &[(6, b"scion-ext")]);
        out.insert(
            "tree_even_extension",
            ModelCase {
                id: make_id(KIND_TREE, &bytes),
                bytes,
            },
        );
    }
    // tree_all_modes' "bin" subtree is tree_subdir.
    assert_eq!(
        tree_bytes_for_dir(&all_modes, b"bin"),
        tree_bytes_for_dir(&subdir, b"")
    );

    let ada = b"Ada Lovelace <ada@example.com>".to_vec();
    let meta_root = MMeta {
        author: b"scion <scion@localhost>".to_vec(),
        author_time: (0, 0),
        committer: None,
        committer_time: None,
        creator: b"service:scion".to_vec(),
        message: b"root".to_vec(),
        ext: vec![],
    };
    let meta_native = MMeta {
        author: ada.clone(),
        author_time: (1_700_000_000, -300),
        committer: None,
        committer_time: None,
        creator: b"github:ada".to_vec(),
        message: b"Add hello\n".to_vec(),
        ext: vec![],
    };
    let meta_with_committer = MMeta {
        author: ada,
        author_time: (1_600_000_000, 120),
        committer: Some(b"Grace Hopper <grace@example.com>".to_vec()),
        committer_time: Some((1_600_000_100, -480)),
        creator: b"service:importer".to_vec(),
        message: b"Imported commit\n\nWith a body.\n".to_vec(),
        ext: vec![],
    };
    let mut meta_git = meta_with_committer.clone();
    meta_git.ext = vec![(2, from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904"))];
    let meta_negative = MMeta {
        author: vec![],
        author_time: (-1, -720),
        committer: None,
        committer_time: Some((i64::MIN, 840)),
        creator: vec![],
        message: vec![],
        ext: vec![],
    };
    out.insert("meta_root", meta_case(&meta_root));
    out.insert("meta_native", meta_case(&meta_native));
    out.insert("meta_with_committer", meta_case(&meta_with_committer));
    out.insert("meta_git_commit_ext", meta_case(&meta_git));
    out.insert("meta_negative_time", meta_case(&meta_negative));

    // First 16 bytes of BLAKE3("scion-root-v1"): the digest part of a refmodel id.
    let root_change_id: [u8; 16] = make_id(KIND_CHANGESET, b"scion-root-v1")[2..18]
        .try_into()
        .unwrap();
    let cs_root = changeset_for(vec![], &empty, &empty, meta_id(&meta_root), root_change_id);
    assert!(cs_root.delta.is_empty());
    let cs_single = changeset_for(
        vec![changeset_id(&cs_root)],
        &empty,
        &single,
        meta_id(&meta_native),
        ascending16(0x00),
    );
    // The description spells the delta out; changeset_for must agree with it.
    assert_eq!(
        cs_single.delta,
        vec![(b"hello.txt".to_vec(), Some((1, blob_id(hello), 6)))]
    );
    let cs_merge = changeset_for(
        vec![changeset_id(&cs_single), changeset_id(&cs_root)],
        &single,
        &all_modes,
        meta_id(&meta_with_committer),
        ascending16(0xa0),
    );
    let delta_paths: Vec<&[u8]> = cs_merge.delta.iter().map(|(p, _)| p.as_slice()).collect();
    assert_eq!(
        delta_paths,
        vec![
            b"README".as_slice(),
            b"bin/inner.txt",
            b"hello.txt",
            b"link",
            b"run.sh"
        ]
    );
    assert_eq!(cs_merge.delta[2].1, None, "hello.txt is deleted");
    out.insert("changeset_root", changeset_case(&cs_root));
    out.insert("changeset_single_parent", changeset_case(&cs_single));
    out.insert("changeset_two_parents_mixed", changeset_case(&cs_merge));
    {
        let bytes = with_extensions(changeset_bytes(&cs_single), &[(8, &[1, 2, 3])]);
        out.insert(
            "changeset_even_extension",
            ModelCase {
                id: make_id(KIND_CHANGESET, &bytes),
                bytes,
            },
        );
    }

    let node_root_dir = history_node_case(2, tree_id(&empty), changeset_id(&cs_root), &[], &[]);
    let node_file = history_node_case(1, blob_id(hello), changeset_id(&cs_single), &[], &[]);
    let node_dir = history_node_case(
        2,
        tree_id(&single),
        changeset_id(&cs_single),
        &[node_root_dir.id],
        &[(b"hello.txt", node_file.id)],
    );
    out.insert("history_node_root_dir", node_root_dir);
    out.insert("history_node_file", node_file);
    out.insert("history_node_dir", node_dir);
    out
}

/// Production id text of a golden vector, decoded and re-identified by scion-object.
fn production_id_text(kind: &str, bytes: &[u8]) -> String {
    match kind {
        "blob" => ObjectId::hash_blob(bytes).to_text(),
        "tree" => Tree::decode(bytes).unwrap().id().to_text(),
        "meta" => Meta::decode(bytes).unwrap().id().to_text(),
        "changeset" => Changeset::decode(bytes).unwrap().id().to_text(),
        "history_node" => scion_object::HistoryNode::decode(bytes)
            .unwrap()
            .id()
            .to_text(),
        other => panic!("unknown golden kind {other}"),
    }
}

#[test]
fn golden_objects_reproduced_by_refmodel_encoder() {
    let model = model_golden_objects();
    let files = load_golden("objects");
    assert!(files.len() >= 14, "at least 14 golden object vectors");
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    for (file, v) in &files {
        let name = field(v, "name");
        let kind = field(v, "kind");
        let want_hex = field(v, "hex");
        let want_id = field(v, "id_text");
        let Some(case) = model.get(name) else {
            failures.push(format!(
                "{file}: no refmodel construction for vector {name}"
            ));
            continue;
        };
        seen.insert(name.to_string());
        if to_hex(&case.bytes) != want_hex {
            failures.push(format!(
                "{file}: refmodel bytes differ\n  refmodel: {}\n  golden:   {want_hex}",
                to_hex(&case.bytes)
            ));
        }
        if id_text(&case.id) != want_id {
            failures.push(format!(
                "{file}: refmodel id {} != golden {want_id}",
                id_text(&case.id)
            ));
        }
        // Both encoders agree through the production decoder as well.
        let prod = production_id_text(kind, &from_hex(want_hex));
        if prod != id_text(&case.id) {
            failures.push(format!(
                "{file}: production id {prod} != refmodel {}",
                id_text(&case.id)
            ));
        }
        // The id is the documented function of the bytes (kind byte from the id).
        let recomputed = if kind == "blob" {
            blob_id(&case.bytes)
        } else {
            make_id(case.id[1], &case.bytes)
        };
        assert_eq!(recomputed, case.id, "{file}");
    }
    let unused: Vec<&&str> = model.keys().filter(|k| !seen.contains(**k)).collect();
    assert!(
        unused.is_empty(),
        "constructions without a vector: {unused:?}"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn golden_kinds_match_refmodel_kind_bytes() {
    // The refmodel's kind bytes are the SPEC 4.2 values production uses.
    assert_eq!(KIND_TREE, Kind::Tree.byte());
    assert_eq!(KIND_META, Kind::Meta.byte());
    assert_eq!(KIND_CHANGESET, Kind::Changeset.byte());
    assert_eq!(blob_id(b"")[1], Kind::Blob.byte());
    assert_eq!(
        *scion_core::empty_tree_id().as_bytes(),
        tree_id(&MTree::new())
    );
}

// =============================================================================================
// 2. Random objects: production id == refmodel id (SPEC 19.1)
// =============================================================================================

fn tree_ids_agree(t: MTree) -> TestCaseResult {
    let prod = prod_tree_objects(&t);
    ensure_eq(
        prod.keys().cloned().collect::<BTreeSet<_>>(),
        dirs_of(&t),
        "directory set",
    )?;
    for (dir, (id, bytes)) in &prod {
        let model = tree_bytes_for_dir(&t, dir);
        ensure_eq(
            Text(bytes.clone()),
            Text(model.clone()),
            &format!("tree bytes of dir {:?}", Text(dir.clone())),
        )?;
        ensure_eq(*id.as_bytes(), make_id(KIND_TREE, &model), "tree id")?;
        // The model bytes are canonical for the production decoder too.
        let decoded = Tree::decode(&model).map_err(|e| TestCaseError::fail(e.to_string()))?;
        ensure_eq(decoded.encode(), model, "re-encode of model bytes")?;
    }
    let root = prod.get(b"".as_slice()).map(|(id, _)| *id.as_bytes());
    ensure_eq(root, Some(tree_id(&t)), "root tree id")?;
    ensure_eq(
        prod.get(b"".as_slice()).map(|(id, _)| id.to_text()),
        Some(id_text(&tree_id(&t))),
        "root id text",
    )?;
    for f in t.values() {
        ensure_eq(
            *ObjectId::hash_blob(&f.content).as_bytes(),
            blob_id(&f.content),
            "blob id",
        )?;
    }
    Ok(())
}

fn meta_ids_agree(m: MMeta) -> TestCaseResult {
    let prod = prod_meta(&m);
    let model = meta_bytes(&m);
    ensure_eq(Text(prod.encode()), Text(model.clone()), "meta bytes")?;
    ensure_eq(*prod.id().as_bytes(), meta_id(&m), "meta id")?;
    ensure_eq(prod.id().to_text(), id_text(&meta_id(&m)), "meta id text")?;
    let decoded = Meta::decode(&model).map_err(|e| TestCaseError::fail(e.to_string()))?;
    ensure_eq(decoded, prod, "decode of model bytes")
}

fn changeset_ids_agree(c: MChangeset) -> TestCaseResult {
    let prod = prod_changeset(&c);
    let model = changeset_bytes(&c);
    ensure_eq(Text(prod.encode()), Text(model.clone()), "changeset bytes")?;
    ensure_eq(*prod.id().as_bytes(), changeset_id(&c), "changeset id")?;
    ensure_eq(
        prod.id().to_text(),
        id_text(&changeset_id(&c)),
        "changeset id text",
    )?;
    let decoded = Changeset::decode(&model).map_err(|e| TestCaseError::fail(e.to_string()))?;
    ensure_eq(decoded, prod, "decode of model bytes")
}

/// A valid changeset (delta = FileDiff(T1, root_tree)) built both ways: the refmodel's
/// `changeset_for` vs production `file_diff` + scion-object trees.
fn valid_changeset_agrees(
    (t1, tree): (MTree, MTree),
    parents: Vec<[u8; 34]>,
    m: MMeta,
    change_id: [u8; 16],
) -> TestCaseResult {
    let model = changeset_for(parents.clone(), &t1, &tree, meta_id(&m), change_id);
    let root = prod_tree_objects(&tree)
        .get(b"".as_slice())
        .map(|(id, _)| *id)
        .unwrap();
    let prod = Changeset {
        parents: parents.iter().map(oid).collect(),
        root_tree: root,
        meta: prod_meta(&m).id(),
        change_id: ChangeId(change_id),
        delta: scion_object::file_diff(&flat_tree(&t1), &flat_tree(&tree)),
        extensions: Default::default(),
    };
    ensure_eq(prod_changeset(&model), prod.clone(), "changeset fields")?;
    ensure_eq(
        Text(prod.encode()),
        Text(changeset_bytes(&model)),
        "changeset bytes",
    )?;
    ensure_eq(*prod.id().as_bytes(), changeset_id(&model), "changeset id")
}

fn arb_valid_changeset_input()
-> impl Strategy<Value = ((MTree, MTree), Vec<[u8; 34]>, MMeta, [u8; 16])> {
    (
        arb_tree_pair(16),
        prop::collection::btree_set(arb_raw_id(3), 0..=3).prop_map(|s| s.into_iter().collect()),
        arb_mmeta(),
        any::<[u8; 16]>(),
    )
}

fn run_object_ids(cases: u32) {
    check("tree ids", cases, arb_tree(24), tree_ids_agree);
    check("meta ids", cases, arb_mmeta(), meta_ids_agree);
    check(
        "changeset ids",
        cases,
        arb_mchangeset(),
        changeset_ids_agree,
    );
    check(
        "valid changeset ids",
        cases,
        arb_valid_changeset_input(),
        |(pair, parents, m, cid)| valid_changeset_agrees(pair, parents, m, cid),
    );
}

#[test]
fn random_object_ids_agree() {
    run_object_ids(default_cases());
}

#[test]
#[ignore = "spec-sized (10^6); run with SCION_FULL=1 via scripts/check.sh --full"]
fn full_random_object_ids_agree() {
    if let Some(n) = full_cases("full_random_object_ids_agree") {
        run_object_ids(n);
    }
}

// =============================================================================================
// 3. Golden diff vectors through the refmodel (line-diff3-v1)
// =============================================================================================

fn json_edits(runs: &[(char, usize, usize, usize)]) -> serde_json::Value {
    serde_json::Value::Array(
        runs.iter()
            .map(|&(op, a, b, len)| match op {
                '=' => serde_json::json!({"op": "equal", "a": a, "b": b, "len": len}),
                '-' => serde_json::json!({"op": "delete", "a": a, "len": len}),
                _ => serde_json::json!({"op": "insert", "b": b, "len": len}),
            })
            .collect(),
    )
}

fn json_hunks(hunks: &[scion_refmodel::diff::MHunk]) -> serde_json::Value {
    serde_json::Value::Array(
        hunks
            .iter()
            .map(|h| serde_json::json!({"base": [h.bs, h.be], "new": [h.ns, h.ne]}))
            .collect(),
    )
}

#[allow(clippy::type_complexity)]
fn json_conflicts(cs: &[((usize, usize), (usize, usize), (usize, usize))]) -> serde_json::Value {
    serde_json::Value::Array(
        cs.iter()
            .map(|(b, o, t)| {
                serde_json::json!({"base": [b.0, b.1], "ours": [o.0, o.1], "theirs": [t.0, t.1]})
            })
            .collect(),
    )
}

#[test]
fn golden_diff_vectors_reproduced_by_refmodel() {
    use scion_refmodel::diff::{diff_hunks, edit_runs, merge3, merge3_full, split_lines};
    let files = load_golden("diff");
    assert!(files.len() >= 60, "at least 60 line-diff3-v1 vectors");
    let mut failures = Vec::new();
    let (mut diffs, mut merges) = (0, 0);
    for (file, v) in &files {
        match field(v, "kind") {
            "diff" => {
                diffs += 1;
                let a = from_hex(field(v, "a_hex"));
                let b = from_hex(field(v, "b_hex"));
                let (al, bl) = (split_lines(&a), split_lines(&b));
                let edits = json_edits(&edit_runs(&al, &bl));
                if Some(&edits) != v.get("edits") {
                    failures.push(format!(
                        "{file}: refmodel edits {edits}\n  golden {}",
                        v["edits"]
                    ));
                }
                let hunks = json_hunks(&diff_hunks(&al, &bl));
                if Some(&hunks) != v.get("hunks") {
                    failures.push(format!(
                        "{file}: refmodel hunks {hunks}\n  golden {}",
                        v["hunks"]
                    ));
                }
            }
            "merge" => {
                merges += 1;
                let base = from_hex(field(v, "base_hex"));
                let ours = from_hex(field(v, "ours_hex"));
                let theirs = from_hex(field(v, "theirs_hex"));
                let full = merge3_full(&base, &ours, &theirs);
                match (&full, v.get("result_hex"), v.get("conflicts")) {
                    (Ok(bytes), Some(want), None) => {
                        if Some(to_hex(bytes).as_str()) != want.as_str() {
                            failures.push(format!(
                                "{file}: refmodel clean result {:?}, golden {want}",
                                Text(bytes.clone())
                            ));
                        }
                    }
                    (Err(cs), None, Some(want)) => {
                        let got = json_conflicts(cs);
                        if &got != want {
                            failures
                                .push(format!("{file}: refmodel conflicts {got}\n  golden {want}"));
                        }
                    }
                    (got, _, _) => failures.push(format!(
                        "{file}: refmodel outcome {got:?} differs from the golden outcome"
                    )),
                }
                // The frozen `merge3` wrapper reports the same outcome and base ranges.
                let wrapped = merge3(&base, &ours, &theirs);
                let expect = full
                    .clone()
                    .map_err(|cs| cs.into_iter().map(|(b, _, _)| b).collect::<Vec<_>>());
                if wrapped != expect {
                    failures.push(format!("{file}: refmodel merge3 != merge3_full"));
                }
            }
            other => failures.push(format!("{file}: unknown kind {other}")),
        }
    }
    assert!(diffs >= 12 && merges >= 12, "diff {diffs}, merge {merges}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// =============================================================================================
// 4. merge3 / myers differential (SPEC 19.4)
// =============================================================================================

fn merge3_agrees(t: Triple) -> TestCaseResult {
    let (b, o, th) = (&t.base.0, &t.ours.0, &t.theirs.0);
    let prod = scion_merge::merge3(b, o, th);
    let model = scion_refmodel::diff::merge3_full(b, o, th);
    match (prod, model) {
        (Merge3::Clean(p), Ok(m)) => ensure_eq(Text(p), Text(m), "clean merge bytes"),
        (Merge3::Conflict(p), Err(m)) => ensure_eq(
            p,
            m.iter().map(conflict_hunk).collect(),
            "conflict hunks (base/ours/theirs ranges)",
        ),
        (p, m) => Err(TestCaseError::fail(format!(
            "clean/conflict outcome differs:\n  production: {p:?}\n  refmodel:   {:?}",
            m.map(Text)
        ))),
    }
}

fn myers_agrees((a, b): (Text, Text)) -> TestCaseResult {
    let pa = scion_merge::split_lines(&a.0);
    let pb = scion_merge::split_lines(&b.0);
    ensure_eq(&pa, &scion_refmodel::diff::split_lines(&a.0), "split_lines")?;
    let model_edits: Vec<Edit> = scion_refmodel::diff::edit_runs(&pa, &pb)
        .into_iter()
        .map(|(op, a, b, len)| match op {
            '=' => Edit::Equal { a, b, len },
            '-' => Edit::Delete { a, len },
            _ => Edit::Insert { b, len },
        })
        .collect();
    ensure_eq(scion_merge::myers(&pa, &pb), model_edits, "myers edit runs")?;
    let model_hunks: Vec<Hunk> = scion_refmodel::diff::diff_hunks(&pa, &pb)
        .into_iter()
        .map(|h| Hunk {
            base: h.bs..h.be,
            new: h.ns..h.ne,
        })
        .collect();
    ensure_eq(scion_merge::diff_lines(&pa, &pb), model_hunks, "diff_lines")?;
    ensure_eq(
        scion_merge::line_origins(&a.0, &b.0),
        scion_refmodel::diff::line_origins(&a.0, &b.0),
        "line_origins",
    )
}

fn run_merge_differential(cases: u32) {
    check("merge3 differential", cases, arb_triple(), merge3_agrees);
    check("myers differential", cases, arb_text_pair(), myers_agrees);
}

#[test]
fn merge3_differential_matches_refmodel() {
    run_merge_differential(default_cases());
}

#[test]
#[ignore = "spec-sized (10^6); run with SCION_FULL=1 via scripts/check.sh --full"]
fn full_merge3_differential_matches_refmodel() {
    if let Some(n) = full_cases("full_merge3_differential_matches_refmodel") {
        run_merge_differential(n);
    }
}

/// Hand-picked triples covering the generator's intent explicitly: CRLF, lone `\r`,
/// unterminated last lines, identical edits, empty sides.
#[test]
fn merge3_differential_handpicked() {
    let cases: &[(&[u8], &[u8], &[u8])] = &[
        (b"", b"", b""),
        (b"", b"x", b"x"),
        (b"", b"x", b"y"),
        (b"a", b"a\n", b"a"),
        (b"a", b"a\n", b"a\r\n"),
        (b"a\nb", b"a\nb\n", b"a\nb\n"),
        (b"a\nb", b"a\nbc", b"a\nb\nc"),
        (b"a\r\nb\r\n", b"a\nb\r\n", b"a\r\nb\n"),
        (b"a\r\nb\r\nc\r\n", b"a\r\nb\r\nc\r", b"A\r\nb\r\nc\r\n"),
        (b"a\nb\nc\n", b"a\nX\nc\n", b"a\nX\nc\n"),
        (b"a\nb\nc\nd\ne\n", b"X\nb\nc\nd\nY\n", b"X\nb\nc\nd\ne\n"),
        (b"a\na\na\n", b"a\na\n", b"a\na\na\na\n"),
        (b"a\nb\na\nb\n", b"b\na\nb\na\n", b"a\nb\n"),
        (b"\n\n\n", b"\n\n", b"\n\n\n\n"),
        (b"x\n", b"", b""),
        (b"x\n", b"", b"y\n"),
    ];
    for (b, o, t) in cases {
        let triple = Triple {
            base: Text(b.to_vec()),
            ours: Text(o.to_vec()),
            theirs: Text(t.to_vec()),
        };
        if let Err(e) = merge3_agrees(triple.clone()) {
            panic!("{triple:?}: {e}");
        }
        if let Err(e) = myers_agrees((Text(b.to_vec()), Text(o.to_vec()))) {
            panic!("myers {triple:?}: {e}");
        }
    }
}

// =============================================================================================
// 5. FileDiff differential (SPEC 4.3.4)
// =============================================================================================

fn file_diff_agrees((a, b): (MTree, MTree)) -> TestCaseResult {
    let prod = scion_object::file_diff(&flat_tree(&a), &flat_tree(&b));
    let model_delta = scion_refmodel::file_diff(&a, &b);
    let model: Vec<_> = model_delta.iter().map(delta_entry).collect();
    ensure_eq(prod, model, "FileDiff")?;
    // The model is self-consistent: applying FileDiff(a, b) to a yields b (SPEC 19.2).
    ensure_eq(
        scion_refmodel::apply_delta(&a, &model_delta),
        Ok(b.clone()),
        "refmodel apply_delta(a, FileDiff(a, b))",
    )?;
    ensure_eq(
        scion_object::file_diff(&flat_tree(&b), &flat_tree(&b)).len(),
        0,
        "FileDiff(b, b) is empty",
    )
}

#[test]
fn file_diff_matches_refmodel() {
    check(
        "file_diff differential",
        default_cases(),
        arb_tree_pair(20),
        file_diff_agrees,
    );
}

#[test]
#[ignore = "spec-sized (10^6); run with SCION_FULL=1 via scripts/check.sh --full"]
fn full_file_diff_matches_refmodel() {
    if let Some(n) = full_cases("full_file_diff_matches_refmodel") {
        check(
            "file_diff differential",
            n,
            arb_tree_pair(20),
            file_diff_agrees,
        );
    }
}

// =============================================================================================
// 6. scion_testkit::encode_tree vs the refmodel trees
// =============================================================================================

fn encode_tree_agrees(t: &MTree) -> TestCaseResult {
    let (root, objects) = encode_tree(t);
    ensure_eq(*root.as_bytes(), tree_id(t), "encode_tree root id")?;
    ensure_eq(
        objects.last().map(|(id, _, _)| *id),
        Some(root),
        "root tree is the last object",
    )?;
    let mut expected: BTreeMap<[u8; 34], (Kind, Vec<u8>)> = BTreeMap::new();
    for dir in dirs_of(t) {
        let bytes = tree_bytes_for_dir(t, &dir);
        expected.insert(make_id(KIND_TREE, &bytes), (Kind::Tree, bytes));
    }
    for f in t.values() {
        expected.insert(blob_id(&f.content), (Kind::Blob, f.content.clone()));
    }
    let got: BTreeMap<[u8; 34], (Kind, Vec<u8>)> = objects
        .iter()
        .map(|(id, k, b)| (*id.as_bytes(), (*k, b.to_vec())))
        .collect();
    ensure_eq(got.len(), objects.len(), "objects are deduplicated by id")?;
    ensure_eq(got, expected, "encode_tree objects")
}

#[test]
fn encode_tree_matches_refmodel_random() {
    check("encode_tree", default_cases(), arb_tree(24), |t| {
        encode_tree_agrees(&t)
    });
}

/// A whole generated history built both ways: refmodel `changeset_for` chain vs production
/// `encode_tree` roots + `file_diff` deltas + scion-object changesets.
fn repo_agrees(label: &str, repo: &GenRepo) {
    let mut model_ids: Vec<[u8; 34]> = Vec::new();
    let mut prod_ids: Vec<ObjectId> = Vec::new();
    for (i, c) in repo.commits.iter().enumerate() {
        if let Err(e) = encode_tree_agrees(&c.tree) {
            panic!("{label} commit {i}: {e}");
        }
        let m = MMeta {
            author: c.author.clone(),
            author_time: (i as i64 * 60, 0),
            committer: None,
            committer_time: None,
            creator: b"service:corpus".to_vec(),
            message: c.message.clone(),
            ext: vec![],
        };
        let empty = MTree::new();
        let t1 = c
            .parents
            .first()
            .map(|&p| &repo.commits[p].tree)
            .unwrap_or(&empty);
        let model = changeset_for(
            c.parents.iter().map(|&p| model_ids[p]).collect(),
            t1,
            &c.tree,
            meta_id(&m),
            c.change_id,
        );
        let prod = Changeset {
            parents: c.parents.iter().map(|&p| prod_ids[p]).collect(),
            root_tree: encode_tree(&c.tree).0,
            meta: prod_meta(&m).id(),
            change_id: ChangeId(c.change_id),
            delta: scion_object::file_diff(&flat_tree(t1), &flat_tree(&c.tree)),
            extensions: Default::default(),
        };
        assert_eq!(
            to_hex(&prod.encode()),
            to_hex(&changeset_bytes(&model)),
            "{label} commit {i}: changeset bytes"
        );
        model_ids.push(changeset_id(&model));
        prod_ids.push(prod.id());
        assert_eq!(prod_ids[i].as_bytes(), &model_ids[i], "{label} commit {i}");
    }
}

#[test]
fn encode_tree_and_changesets_match_refmodel_on_corpus() {
    for (name, repo) in edge_cases() {
        repo_agrees(name, &repo);
    }
    for seed in 0..3u64 {
        let spec = CorpusSpec {
            files: 60,
            history_len: 25,
            seed,
            ..CorpusSpec::default()
        };
        repo_agrees(&format!("corpus seed {seed}"), &generate(&spec));
    }
}
