//! Shared helpers for the oracle cross-validation suite (P17; SPEC 19.0, 19.1, 19.4, 4.3.4).
//!
//! - Golden-vector loading and hex helpers.
//! - Conversions between the reference model's structural values (`scion_refmodel`) and the
//!   production entities (`scion_object`, `scion_merge`).
//! - Proptest strategies for valid structural trees, related tree pairs, Metas, changesets,
//!   line sequences and three-way text triples (CRLF, unterminated last lines, identical
//!   edits).
//! - [`check`]: a deterministic proptest runner that panics with the *minimized* failing case,
//!   so a disagreement between the oracle and production code is reported in its smallest form.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner};
use scion_core::test_support::{full_mode, proptest_cases};
use scion_core::{ChangeId, ConflictHunk, EntryInfo, Kind, Mode, ObjectId};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, FlatTree, Meta, Name, RepoPath,
    Timestamp, Tree, TreeEntry,
};
use scion_refmodel::{MChangeset, MDelta, MFile, MMeta, MPath, MTree};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

// ---------------------------------------------------------------------------------------------
// Case counts and the runner
// ---------------------------------------------------------------------------------------------

/// Default property case count (PLAN 3.5: about 10^4 where SPEC 19.1/19.4 demand 10^6).
pub fn default_cases() -> u32 {
    proptest_cases(10_000)
}

/// Spec-sized count for the `#[ignore] full_*` variants, or `None` (with a printed `SKIP:`)
/// when `SCION_FULL=1` is not set.
pub fn full_cases(test: &str) -> Option<u32> {
    if full_mode() {
        Some(1_000_000)
    } else {
        println!("SKIP: {test} needs SCION_FULL=1 (spec-sized 10^6 cases)");
        None
    }
}

/// Fixed default seed so failures reproduce; `SCION_ORACLE_SEED=<u64>` explores other seeds.
fn seed_bytes() -> [u8; 32] {
    let seed: u64 = std::env::var("SCION_ORACLE_SEED")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0x5c10_170a_c1e5);
    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&(seed.wrapping_add(i as u64)).to_le_bytes());
    }
    out
}

/// Run `test` on `cases` values of `strategy`. On a disagreement, proptest shrinks the input
/// and this panics with the minimized case (Debug) and the failure reason.
pub fn check<S>(name: &str, cases: u32, strategy: S, test: impl Fn(S::Value) -> TestCaseResult)
where
    S: Strategy,
    S::Value: fmt::Debug,
{
    let mut config = Config::with_cases(cases);
    config.failure_persistence = None;
    config.source_file = None;
    config.max_global_rejects = cases.saturating_mul(4).max(1024);
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes());
    let mut runner = TestRunner::new_with_rng(config, rng);
    match runner.run(&strategy, test) {
        Ok(()) => {}
        Err(TestError::Fail(reason, value)) => {
            panic!("{name}: oracle disagreement; minimized case:\n{value:#?}\nreason: {reason}")
        }
        Err(TestError::Abort(reason)) => panic!("{name}: aborted: {reason}"),
    }
}

/// `prop_assert_eq!` replacement usable in helper functions returning [`TestCaseResult`].
pub fn ensure_eq<T: PartialEq + fmt::Debug>(left: T, right: T, what: &str) -> TestCaseResult {
    if left == right {
        Ok(())
    } else {
        Err(TestCaseError::fail(format!(
            "{what}:\n  production/left: {left:?}\n  refmodel/right:  {right:?}"
        )))
    }
}

/// Test case result alias.
pub type TestCaseResult = Result<(), TestCaseError>;

// ---------------------------------------------------------------------------------------------
// Hex, golden files
// ---------------------------------------------------------------------------------------------

/// Lowercase hex of `b`.
pub fn to_hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(DIGITS[(x >> 4) as usize] as char);
        s.push(DIGITS[(x & 0xf) as usize] as char);
    }
    s
}

/// Decode lowercase hex (panics on malformed input; golden files are frozen).
pub fn from_hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length");
    let nib = |c: u8| -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex digit {c:#x}"),
        }
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| (nib(p[0]) << 4) | nib(p[1]))
        .collect()
}

/// `testdata/golden/<sub>` at the repository root.
pub fn golden_dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/golden")
        .join(sub)
}

/// Every `*.json` file of a golden directory as `(file name, parsed value)`, sorted by name.
pub fn load_golden(sub: &str) -> Vec<(String, serde_json::Value)> {
    let dir = golden_dir(sub);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}")) {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        out.push((name, serde_json::from_str(&text).unwrap()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A string field of a golden case.
pub fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_else(|| panic!("missing string field {key}"))
}

// ---------------------------------------------------------------------------------------------
// Structural <-> production conversions
// ---------------------------------------------------------------------------------------------

/// A structural file.
pub fn mfile(mode: u8, content: &[u8]) -> MFile {
    MFile {
        mode,
        content: content.to_vec(),
    }
}

/// Production `ObjectId` of a raw refmodel id.
pub fn oid(raw: &[u8; 34]) -> ObjectId {
    ObjectId::from_bytes(raw).unwrap()
}

/// Production `EntryInfo` of a structural file (blob id computed by scion-core).
pub fn entry_info(f: &MFile) -> EntryInfo {
    EntryInfo {
        mode: Mode::from_u8(f.mode).unwrap(),
        id: ObjectId::hash_blob(&f.content),
        size: f.content.len() as u64,
    }
}

/// Production flat tree of a structural tree.
pub fn flat_tree(t: &MTree) -> FlatTree {
    t.iter()
        .map(|(p, f)| (RepoPath::parse(p).unwrap(), entry_info(f)))
        .collect()
}

/// Production `DeltaEntry` of a structural delta entry.
pub fn delta_entry(d: &MDelta) -> DeltaEntry {
    match d {
        MDelta::Put(p, f) => {
            let e = entry_info(f);
            DeltaEntry {
                path: RepoPath::parse(p).unwrap(),
                op: DeltaOp::Put {
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                },
            }
        }
        MDelta::Delete(p) => DeltaEntry {
            path: RepoPath::parse(p).unwrap(),
            op: DeltaOp::Delete,
        },
    }
}

/// Production `Meta` of a structural Meta.
pub fn prod_meta(m: &MMeta) -> Meta {
    let mut extensions = Extensions::new();
    for (tag, value) in &m.ext {
        extensions.insert(*tag, value.clone()).unwrap();
    }
    let ts = |(seconds, tz_offset_minutes): (i64, i64)| Timestamp {
        seconds,
        tz_offset_minutes,
    };
    Meta {
        author: m.author.clone(),
        author_time: ts(m.author_time),
        committer: m.committer.clone(),
        committer_time: m.committer_time.map(ts),
        creator: m.creator.clone(),
        message: m.message.clone(),
        extensions,
    }
}

/// Production `Changeset` of a structural changeset (no extensions).
pub fn prod_changeset(c: &MChangeset) -> Changeset {
    Changeset {
        parents: c.parents.iter().map(oid).collect(),
        root_tree: oid(&c.root_tree),
        meta: oid(&c.meta),
        change_id: ChangeId(c.change_id),
        delta: c
            .delta
            .iter()
            .map(|(p, op)| DeltaEntry {
                path: RepoPath::parse(p).unwrap(),
                op: match op {
                    Some((mode, id, size)) => DeltaOp::Put {
                        mode: Mode::from_u8(*mode).unwrap(),
                        id: oid(id),
                        size: *size,
                    },
                    None => DeltaOp::Delete,
                },
            })
            .collect(),
        extensions: Extensions::new(),
    }
}

/// Every directory of a structural tree (the root `b""` included), as paths.
pub fn dirs_of(t: &MTree) -> BTreeSet<Vec<u8>> {
    let mut dirs = BTreeSet::new();
    dirs.insert(Vec::new());
    for p in t.keys() {
        for (i, &c) in p.iter().enumerate() {
            if c == b'/' {
                dirs.insert(p[..i].to_vec());
            }
        }
    }
    dirs
}

/// Production encoding of every directory of `t`, built here directly with scion-object
/// (independent of `scion_testkit::encode_tree`): `dir -> (id, canonical bytes)`.
pub fn prod_tree_objects(t: &MTree) -> BTreeMap<Vec<u8>, (ObjectId, Vec<u8>)> {
    fn build(t: &MTree, dir: &[u8], out: &mut BTreeMap<Vec<u8>, (ObjectId, Vec<u8>)>) -> ObjectId {
        let prefix: Vec<u8> = if dir.is_empty() {
            Vec::new()
        } else {
            let mut p = dir.to_vec();
            p.push(b'/');
            p
        };
        let mut entries: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
        for (path, f) in t.range(prefix.clone()..) {
            if !path.starts_with(&prefix) {
                break;
            }
            let rest = &path[prefix.len()..];
            match rest.iter().position(|&c| c == b'/') {
                None => {
                    let e = entry_info(f);
                    entries.insert(
                        rest.to_vec(),
                        TreeEntry {
                            name: Name::new(rest).unwrap(),
                            mode: e.mode,
                            id: e.id,
                            size: e.size,
                        },
                    );
                }
                Some(i) => {
                    let name = &rest[..i];
                    if !entries.contains_key(name) {
                        let mut sub = prefix.clone();
                        sub.extend_from_slice(name);
                        let id = build(t, &sub, out);
                        entries.insert(
                            name.to_vec(),
                            TreeEntry {
                                name: Name::new(name).unwrap(),
                                mode: Mode::Tree,
                                id,
                                size: 0,
                            },
                        );
                    }
                }
            }
        }
        let tree = Tree {
            entries: entries.into_values().collect(),
            extensions: Extensions::new(),
        };
        let bytes = tree.encode();
        let id = tree.id();
        assert_eq!(id, ObjectId::hash_encoded(Kind::Tree, &bytes));
        out.insert(dir.to_vec(), (id, bytes));
        id
    }
    let mut out = BTreeMap::new();
    build(t, b"", &mut out);
    out
}

/// Refmodel conflict triple as production `ConflictHunk`.
pub fn conflict_hunk(c: &((usize, usize), (usize, usize), (usize, usize))) -> ConflictHunk {
    let r = |(s, e): (usize, usize)| (s as u32)..(e as u32);
    ConflictHunk {
        base: r(c.0),
        ours: r(c.1),
        theirs: r(c.2),
    }
}

// ---------------------------------------------------------------------------------------------
// Tree strategies
// ---------------------------------------------------------------------------------------------

/// Name bytes include values below and above `/` (0x2f) so that flat path order ("a-b" <
/// "a/b") and tree entry order ("a" < "a-b") differ, plus non-UTF-8 bytes.
const NAME_BYTES: &[u8] = b"ab-.A !~_\xff\xc3";

/// A valid SPEC 4.4 name of 1..=3 bytes (`.` and `..` are made valid by prefixing `n`).
pub fn arb_name() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(NAME_BYTES), 1..=3).prop_map(|mut n| {
        if n == b"." || n == b".." {
            n.insert(0, b'n');
        }
        n
    })
}

/// A valid path of 1..=4 components.
pub fn arb_path() -> impl Strategy<Value = MPath> {
    prop::collection::vec(arb_name(), 1..=4).prop_map(|cs| cs.join(&b'/'))
}

/// Small contents (repeats are likely, so blob ids are shared across paths).
pub fn arb_content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => prop::collection::vec(prop::sample::select(b"xy\n\r\0".as_slice()), 0..6),
        1 => prop::collection::vec(any::<u8>(), 0..40),
    ]
}

/// A structural file with mode 1..=3.
pub fn arb_file() -> impl Strategy<Value = MFile> {
    (1u8..=3, arb_content()).prop_map(|(mode, content)| MFile { mode, content })
}

/// Insert `p` unless it conflicts structurally (an ancestor is a file, or `p` is a directory
/// of an existing file). Returns whether it was inserted.
pub fn insert_valid(t: &mut MTree, p: MPath, f: MFile) -> bool {
    for (i, &c) in p.iter().enumerate() {
        if c == b'/' && t.contains_key(&p[..i]) {
            return false;
        }
    }
    let mut lo = p.clone();
    lo.push(b'/');
    let mut hi = p.clone();
    hi.push(b'/' + 1);
    if t.range(lo..hi).next().is_some() {
        return false;
    }
    t.insert(p, f);
    true
}

/// A structurally valid tree from a list of candidate files.
pub fn tree_from(files: Vec<(MPath, MFile)>) -> MTree {
    let mut t = MTree::new();
    for (p, f) in files {
        insert_valid(&mut t, p, f);
    }
    t
}

/// A random structurally valid file-level tree with up to `max` files.
pub fn arb_tree(max: usize) -> impl Strategy<Value = MTree> {
    prop::collection::vec((arb_path(), arb_file()), 0..=max).prop_map(tree_from)
}

/// One mutation applied to a tree to derive a related tree.
#[derive(Clone, Debug)]
pub enum Mutation {
    /// Delete the selected file.
    Delete(prop::sample::Index),
    /// Change the selected file's mode (content unchanged).
    Mode(prop::sample::Index, u8),
    /// Replace the selected file's content.
    Content(prop::sample::Index, Vec<u8>),
    /// File -> directory: delete the file and create `file/name`.
    FileToDir(prop::sample::Index, Vec<u8>, MFile),
    /// Directory -> file: delete everything under the selected file's parent directory and
    /// put a file at the directory path.
    DirToFile(prop::sample::Index, MFile),
    /// Re-put the selected file unchanged (a no-op).
    Touch(prop::sample::Index),
    /// Add a new file (skipped when it conflicts).
    Add(MPath, MFile),
}

fn arb_mutation() -> impl Strategy<Value = Mutation> {
    let idx = any::<prop::sample::Index>;
    prop_oneof![
        2 => idx().prop_map(Mutation::Delete),
        1 => (idx(), 1u8..=3).prop_map(|(i, m)| Mutation::Mode(i, m)),
        2 => (idx(), arb_content()).prop_map(|(i, c)| Mutation::Content(i, c)),
        1 => (idx(), arb_name(), arb_file()).prop_map(|(i, n, f)| Mutation::FileToDir(i, n, f)),
        1 => (idx(), arb_file()).prop_map(|(i, f)| Mutation::DirToFile(i, f)),
        1 => idx().prop_map(Mutation::Touch),
        2 => (arb_path(), arb_file()).prop_map(|(p, f)| Mutation::Add(p, f)),
    ]
}

/// Apply mutations to a copy of `a`; the result stays structurally valid.
pub fn mutate(a: &MTree, muts: &[Mutation]) -> MTree {
    let mut t = a.clone();
    for m in muts {
        let keys: Vec<MPath> = t.keys().cloned().collect();
        let pick = |i: &prop::sample::Index| -> Option<MPath> {
            if keys.is_empty() {
                None
            } else {
                Some(keys[i.index(keys.len())].clone())
            }
        };
        match m {
            Mutation::Delete(i) => {
                if let Some(p) = pick(i) {
                    t.remove(&p);
                }
            }
            Mutation::Mode(i, mode) => {
                if let Some(p) = pick(i)
                    && let Some(f) = t.get_mut(&p)
                {
                    f.mode = *mode;
                }
            }
            Mutation::Content(i, c) => {
                if let Some(p) = pick(i)
                    && let Some(f) = t.get_mut(&p)
                {
                    f.content = c.clone();
                }
            }
            Mutation::FileToDir(i, name, f) => {
                if let Some(p) = pick(i) {
                    let mut child = p.clone();
                    child.push(b'/');
                    child.extend_from_slice(name);
                    if child.len() <= 4096 {
                        t.remove(&p);
                        insert_valid(&mut t, child, f.clone());
                    }
                }
            }
            Mutation::DirToFile(i, f) => {
                if let Some(p) = pick(i)
                    && let Some(slash) = p.iter().rposition(|&c| c == b'/')
                {
                    let dir = p[..slash].to_vec();
                    let mut lo = dir.clone();
                    lo.push(b'/');
                    let mut hi = dir.clone();
                    hi.push(b'/' + 1);
                    let under: Vec<MPath> = t.range(lo..hi).map(|(k, _)| k.clone()).collect();
                    for k in under {
                        t.remove(&k);
                    }
                    insert_valid(&mut t, dir, f.clone());
                }
            }
            Mutation::Touch(i) => {
                if let Some(p) = pick(i)
                    && let Some(f) = t.get(&p).cloned()
                {
                    t.insert(p, f);
                }
            }
            Mutation::Add(p, f) => {
                insert_valid(&mut t, p.clone(), f.clone());
            }
        }
    }
    t
}

/// A pair of related trees `(a, b)`: `b` is `a` after random mutations (deletes, mode and
/// content changes, file <-> directory flips, no-op re-puts, additions).
pub fn arb_tree_pair(max: usize) -> impl Strategy<Value = (MTree, MTree)> {
    (
        arb_tree(max),
        prop::collection::vec(arb_mutation(), 0..10),
        any::<bool>(),
    )
        .prop_map(|(a, muts, unrelated)| {
            let b = mutate(&a, &muts);
            if unrelated && muts.len() > 8 {
                // Occasionally diff against the empty tree (the root changeset's first parent).
                (MTree::new(), b)
            } else {
                (a, b)
            }
        })
}

// ---------------------------------------------------------------------------------------------
// Meta and changeset strategies
// ---------------------------------------------------------------------------------------------

fn arb_bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=max)
}

fn arb_i64() -> impl Strategy<Value = i64> {
    prop_oneof![
        2 => any::<i64>(),
        1 => -2000i64..2000,
        1 => prop::sample::select(vec![0i64, -1, 1, i64::MIN, i64::MAX, 63, -64, 64, -65]),
    ]
}

fn arb_ts() -> impl Strategy<Value = (i64, i64)> {
    (arb_i64(), arb_i64())
}

/// A random structural Meta. Extension tags are even (ignorable), strictly ascending; values
/// may exceed 127 bytes (two-byte length varints).
pub fn arb_mmeta() -> impl Strategy<Value = MMeta> {
    (
        arb_bytes(40),
        arb_ts(),
        prop::option::of(arb_bytes(40)),
        prop::option::of(arb_ts()),
        arb_bytes(24),
        prop_oneof![3 => arb_bytes(60), 1 => arb_bytes(300)],
        prop::collection::btree_map(
            prop_oneof![
                3 => (0u64..8).prop_map(|t| t * 2),
                1 => any::<u64>().prop_map(|t| t & !1),
            ],
            arb_bytes(140),
            0..4,
        ),
    )
        .prop_map(
            |(author, author_time, committer, committer_time, creator, message, ext)| MMeta {
                author,
                author_time,
                committer,
                committer_time,
                creator,
                message,
                ext: ext.into_iter().collect(),
            },
        )
}

/// A random raw id of `kind` (algo BLAKE3).
pub fn arb_raw_id(kind: u8) -> impl Strategy<Value = [u8; 34]> {
    any::<[u8; 32]>().prop_map(move |d| {
        let mut id = [0u8; 34];
        id[0] = 1;
        id[1] = kind;
        id[2..].copy_from_slice(&d);
        id
    })
}

fn arb_size() -> impl Strategy<Value = u64> {
    prop_oneof![3 => 0u64..300, 1 => any::<u64>()]
}

/// A random structural changeset: 0..=4 unique parents, random ids of the right kinds, and a
/// delta that is strictly ascending by path (Put modes 1..=3 and Deletes).
pub fn arb_mchangeset() -> impl Strategy<Value = MChangeset> {
    (
        prop::collection::btree_set(arb_raw_id(3), 0..=4),
        arb_raw_id(2),
        arb_raw_id(4),
        any::<[u8; 16]>(),
        prop::collection::btree_map(
            arb_path(),
            prop::option::of((1u8..=3, arb_raw_id(1), arb_size())),
            0..10,
        ),
        any::<prop::sample::Index>(),
    )
        .prop_map(|(parents, root_tree, meta, change_id, delta, rot)| {
            // Parents are ordered but not sorted: rotate the set so order is arbitrary.
            let mut parents: Vec<[u8; 34]> = parents.into_iter().collect();
            if !parents.is_empty() {
                let k = rot.index(parents.len());
                parents.rotate_left(k);
            }
            MChangeset {
                parents,
                root_tree,
                meta,
                change_id,
                delta: delta.into_iter().collect(),
            }
        })
}

// ---------------------------------------------------------------------------------------------
// Line and text strategies (line-diff3-v1)
// ---------------------------------------------------------------------------------------------

/// Bytes shown escaped in Debug output, so minimized cases are readable.
#[derive(Clone, PartialEq, Eq)]
pub struct Text(pub Vec<u8>);

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b\"{}\"", self.0.escape_ascii())
    }
}

/// One line from a small alphabet (so repeated lines and Myers tie-breaks are common), with an
/// LF (weight 3) or CRLF (weight 1) terminator; the word may be empty (blank lines).
pub fn arb_line() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(vec!["a", "b", "c", "d", ""]),
        prop_oneof![3 => Just("\n"), 1 => Just("\r\n")],
    )
        .prop_map(|(w, t)| format!("{w}{t}").into_bytes())
}

/// Ending tweak applied to the final bytes: strip the last `\n` (leaving a lone `\r` after
/// CRLF), strip the whole terminator, or leave it.
fn tweak_end(mut b: Vec<u8>, tweak: u8) -> Vec<u8> {
    if tweak <= 1 && b.last() == Some(&b'\n') {
        b.pop();
        if tweak == 1 && b.last() == Some(&b'\r') {
            b.pop();
        }
    }
    b
}

/// A line-level edit of one base position.
#[derive(Clone, Debug)]
pub enum LineOp {
    /// Keep the line.
    Keep,
    /// Delete the line.
    Delete,
    /// Replace the line with these lines.
    Replace(Vec<Vec<u8>>),
    /// Insert these lines before the line (or at the end, past the last line).
    Insert(Vec<Vec<u8>>),
}

fn arb_line_op() -> impl Strategy<Value = LineOp> {
    let lines = || prop::collection::vec(arb_line(), 1..=3);
    prop_oneof![
        6 => Just(LineOp::Keep),
        1 => Just(LineOp::Delete),
        1 => lines().prop_map(LineOp::Replace),
        1 => lines().prop_map(LineOp::Insert),
    ]
}

/// Apply per-position ops to a line sequence; missing ops mean Keep.
pub fn apply_ops(lines: &[&[u8]], ops: &[LineOp]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..=lines.len() {
        let line = lines.get(i);
        match ops.get(i).unwrap_or(&LineOp::Keep) {
            LineOp::Keep => {
                if let Some(l) = line {
                    out.extend_from_slice(l);
                }
            }
            LineOp::Delete => {}
            LineOp::Replace(new) => {
                if line.is_some() {
                    for l in new {
                        out.extend_from_slice(l);
                    }
                }
            }
            LineOp::Insert(new) => {
                for l in new {
                    out.extend_from_slice(l);
                }
                if let Some(l) = line {
                    out.extend_from_slice(l);
                }
            }
        }
    }
    out
}

/// A three-way merge input.
#[derive(Clone, Debug)]
pub struct Triple {
    /// Base bytes.
    pub base: Text,
    /// Ours bytes.
    pub ours: Text,
    /// Theirs bytes.
    pub theirs: Text,
}

/// Generated text triples: random line-level edits from a common base, including CRLF lines,
/// unterminated (and lone-`\r`) last lines, identical edits on both sides, one side equal to
/// the base, theirs derived from ours, and sides that share some edits but not others.
pub fn arb_triple() -> impl Strategy<Value = Triple> {
    (
        prop::collection::vec(arb_line(), 0..=12),
        0u8..6,
        prop::collection::vec(arb_line_op(), 0..=14),
        prop::collection::vec(arb_line_op(), 0..=14),
        0u8..10,
        0u8..8,
        0u8..8,
    )
        .prop_map(
            |(base_lines, base_end, ops_o, ops_t, shape, end_o, end_t)| {
                let base = tweak_end(base_lines.concat(), base_end);
                let bl = scion_refmodel::diff::split_lines(&base);
                let ours = tweak_end(apply_ops(&bl, &ops_o), end_o);
                let theirs = match shape {
                    // Identical edits on both sides.
                    6 => ours.clone(),
                    // Theirs edits ours further (shares all of ours' hunks plus its own).
                    7 => {
                        let ol = scion_refmodel::diff::split_lines(&ours);
                        tweak_end(apply_ops(&ol, &ops_t), end_t)
                    }
                    // Theirs is the base.
                    8 => base.clone(),
                    // Theirs shares ours' edits at even positions only.
                    9 => {
                        let mixed: Vec<LineOp> = (0..ops_o.len().max(ops_t.len()))
                            .map(|i| {
                                let src = if i % 2 == 0 { &ops_o } else { &ops_t };
                                src.get(i).cloned().unwrap_or(LineOp::Keep)
                            })
                            .collect();
                        tweak_end(apply_ops(&bl, &mixed), end_t)
                    }
                    // Independent edits.
                    _ => tweak_end(apply_ops(&bl, &ops_t), end_t),
                };
                Triple {
                    base: Text(base),
                    ours: Text(ours),
                    theirs: Text(theirs),
                }
            },
        )
}

/// A pair of byte strings for two-way diffs: `b` is a line-level edit of `a` (or unrelated).
pub fn arb_text_pair() -> impl Strategy<Value = (Text, Text)> {
    (
        prop::collection::vec(arb_line(), 0..=16),
        0u8..6,
        prop::collection::vec(arb_line_op(), 0..=18),
        0u8..8,
        prop::collection::vec(arb_line(), 0..=10),
        0u8..8,
    )
        .prop_map(|(a_lines, a_end, ops, b_end, other, shape)| {
            let a = tweak_end(a_lines.concat(), a_end);
            let b = if shape == 0 {
                tweak_end(other.concat(), b_end)
            } else {
                let al = scion_refmodel::diff::split_lines(&a);
                tweak_end(apply_ops(&al, &ops), b_end)
            };
            (Text(a), Text(b))
        })
}
