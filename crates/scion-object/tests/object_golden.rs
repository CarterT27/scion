//! SPEC 19.1 golden vectors: every object kind's canonical bytes and id are frozen in
//! `testdata/golden/objects/*.json`.
//!
//! - `golden_vectors_match_constructors` rebuilds every case from Rust constructors and
//!   compares bytes and id text both ways (encode == file; decode(file) == constructor).
//! - `golden_files_frozen` asserts a BLAKE3 digest over the sorted file contents, so any edit
//!   to the files fails CI. Changing a vector requires an amendment.
//! - `regenerate_golden_vectors` (ignored) rewrites the files when `SCION_REGEN_GOLDEN=1`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::{ChangeId, EntryInfo, Kind, Mode, ObjectId, empty_tree_id};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, FlatTree, HistoryNode,
    META_EXT_GIT_COMMIT, Meta, Name, NodeKind, RepoPath, Timestamp, Tree, TreeEntry, file_diff,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// BLAKE3 over the sorted golden files (see `digest_of_dir`). Update only via amendment.
const GOLDEN_DIGEST: &str = "34c2137ac1a44be1aeb8d28cd053cdeec01d2f67bada6348f5f641f74e131bca";

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct GoldenFile {
    name: String,
    kind: String,
    description: String,
    hex: String,
    id_text: String,
}

enum Obj {
    Blob(Vec<u8>),
    Tree(Tree),
    Meta(Meta),
    Changeset(Changeset),
    Node(HistoryNode),
}

impl Obj {
    fn kind(&self) -> &'static str {
        match self {
            Obj::Blob(_) => "blob",
            Obj::Tree(_) => "tree",
            Obj::Meta(_) => "meta",
            Obj::Changeset(_) => "changeset",
            Obj::Node(_) => "history_node",
        }
    }
    fn bytes(&self) -> Vec<u8> {
        match self {
            Obj::Blob(b) => b.clone(),
            Obj::Tree(x) => x.encode(),
            Obj::Meta(x) => x.encode(),
            Obj::Changeset(x) => x.encode(),
            Obj::Node(x) => x.encode(),
        }
    }
    fn id(&self) -> ObjectId {
        match self {
            Obj::Blob(b) => ObjectId::hash_blob(b),
            Obj::Tree(x) => x.id(),
            Obj::Meta(x) => x.id(),
            Obj::Changeset(x) => x.id(),
            Obj::Node(x) => x.id(),
        }
    }
    /// decode(bytes) equals the constructor, and re-encoding is byte-identical.
    fn check_decode(&self, bytes: &[u8]) {
        match self {
            Obj::Blob(b) => assert_eq!(b.as_slice(), bytes),
            Obj::Tree(x) => assert_eq!(&Tree::decode(bytes).unwrap(), x),
            Obj::Meta(x) => assert_eq!(&Meta::decode(bytes).unwrap(), x),
            Obj::Changeset(x) => assert_eq!(&Changeset::decode(bytes).unwrap(), x),
            Obj::Node(x) => assert_eq!(&HistoryNode::decode(bytes).unwrap(), x),
        }
    }
}

struct Case {
    name: &'static str,
    description: &'static str,
    obj: Obj,
}

fn name(s: &[u8]) -> Name {
    Name::new(s).unwrap()
}

fn path(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

fn file_entry(n: &[u8], mode: Mode, content: &[u8]) -> TreeEntry {
    TreeEntry {
        name: name(n),
        mode,
        id: ObjectId::hash_blob(content),
        size: content.len() as u64,
    }
}

fn tree_entry(n: &[u8], t: &Tree) -> TreeEntry {
    TreeEntry {
        name: name(n),
        mode: Mode::Tree,
        id: t.id(),
        size: 0,
    }
}

fn change_id(first: u8) -> ChangeId {
    let mut b = [0u8; 16];
    for (i, x) in b.iter_mut().enumerate() {
        *x = first + i as u8;
    }
    ChangeId(b)
}

/// Flatten a tree whose subtrees are given by id (test helper for FileDiff).
fn flatten(t: &Tree, subtrees: &BTreeMap<ObjectId, Tree>, prefix: &RepoPath, out: &mut FlatTree) {
    for e in &t.entries {
        let p = prefix.join(&e.name).unwrap();
        if e.mode == Mode::Tree {
            flatten(&subtrees[&e.id], subtrees, &p, out);
        } else {
            out.insert(
                p,
                EntryInfo {
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                },
            );
        }
    }
}

const HELLO: &[u8] = b"hello\n";
const INNER: &[u8] = b"inner\n";
const SCRIPT: &[u8] = b"#!/bin/sh\necho hi\n";
const LINK: &[u8] = b"../target";

fn cases() -> Vec<Case> {
    let mut v = Vec::new();

    // ---- blobs ----
    v.push(Case {
        name: "blob_empty",
        description: "Blob with empty content; hex is the raw content, id is BLAKE3(raw) with kind Blob.",
        obj: Obj::Blob(Vec::new()),
    });
    v.push(Case {
        name: "blob_hello",
        description: "Blob with content \"hello\\n\" (6 bytes); hex is the raw content, id is BLAKE3(raw) with kind Blob.",
        obj: Obj::Blob(HELLO.to_vec()),
    });

    // ---- trees ----
    v.push(Case {
        name: "tree_empty",
        description: "The empty tree (valid only as a root): format_version 1, no entries, no extensions. Equals scion_core::empty_tree_id().",
        obj: Obj::Tree(Tree::empty()),
    });
    let single = Tree {
        entries: vec![file_entry(b"hello.txt", Mode::File, HELLO)],
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "tree_single_file",
        description: "Tree with one entry: \"hello.txt\" mode 1 (File), id = blob id of \"hello\\n\", size 6. No extensions.",
        obj: Obj::Tree(single.clone()),
    });
    let subdir = Tree {
        entries: vec![file_entry(b"inner.txt", Mode::File, INNER)],
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "tree_subdir",
        description: "Tree with one entry: \"inner.txt\" mode 1 (File), id = blob id of \"inner\\n\", size 6. Used as the \"bin\" subtree of tree_all_modes.",
        obj: Obj::Tree(subdir.clone()),
    });
    let all_modes = Tree {
        entries: vec![
            file_entry(b"README", Mode::File, HELLO),
            tree_entry(b"bin", &subdir),
            file_entry(b"link", Mode::Symlink, LINK),
            file_entry(b"run.sh", Mode::Executable, SCRIPT),
        ],
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "tree_all_modes",
        description: "Tree with all four modes, bytewise sorted: \"README\" mode 1 blob(\"hello\\n\") size 6; \"bin\" mode 4 id = tree_subdir id size 0; \"link\" mode 3 blob(\"../target\") size 9; \"run.sh\" mode 2 blob(\"#!/bin/sh\\necho hi\\n\") size 18.",
        obj: Obj::Tree(all_modes.clone()),
    });
    let mut ext_tree = single.clone();
    ext_tree
        .extensions
        .insert(6, b"scion-ext".to_vec())
        .unwrap();
    v.push(Case {
        name: "tree_even_extension",
        description: "tree_single_file plus one unknown even (ignorable) extension: tag 6, value \"scion-ext\". Preserved byte for byte; changes the id.",
        obj: Obj::Tree(ext_tree),
    });
    let big = vec![b'x'; 200];
    let bytewise = Tree {
        entries: vec![
            file_entry(b"B", Mode::File, b"B"),
            file_entry(b"a", Mode::File, b"a"),
            file_entry("\u{e9}".as_bytes(), Mode::File, &big),
            file_entry(&[0xff], Mode::Executable, b""),
        ],
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "tree_bytewise_names",
        description: "Bytewise name order with non-ASCII names: \"B\" mode 1 blob(\"B\") size 1; \"a\" mode 1 blob(\"a\") size 1; [0xc3,0xa9] (UTF-8 e-acute) mode 1 blob(200 x 'x') size 200 (two-byte varint); [0xff] (not UTF-8) mode 2 blob(\"\") size 0.",
        obj: Obj::Tree(bytewise),
    });

    // ---- metas ----
    let meta_root = Meta {
        author: b"scion <scion@localhost>".to_vec(),
        author_time: Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"service:scion".to_vec(),
        message: b"root".to_vec(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "meta_root",
        description: "Bootstrap Meta (PLAN 6.5): author \"scion <scion@localhost>\", author_time 0/0, no committer, no committer_time, creator \"service:scion\", message \"root\", no extensions.",
        obj: Obj::Meta(meta_root.clone()),
    });
    let meta_native = Meta {
        author: b"Ada Lovelace <ada@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: -300,
        },
        committer: None,
        committer_time: None,
        creator: b"github:ada".to_vec(),
        message: b"Add hello\n".to_vec(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "meta_native",
        description: "Native Meta without committer: author \"Ada Lovelace <ada@example.com>\", author_time seconds 1700000000 tz -300, committer absent, committer_time absent, creator \"github:ada\", message \"Add hello\\n\", no extensions.",
        obj: Obj::Meta(meta_native.clone()),
    });
    let meta_committer = Meta {
        author: b"Ada Lovelace <ada@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_600_000_000,
            tz_offset_minutes: 120,
        },
        committer: Some(b"Grace Hopper <grace@example.com>".to_vec()),
        committer_time: Some(Timestamp {
            seconds: 1_600_000_100,
            tz_offset_minutes: -480,
        }),
        creator: b"service:importer".to_vec(),
        message: b"Imported commit\n\nWith a body.\n".to_vec(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "meta_with_committer",
        description: "Meta with committer: author \"Ada Lovelace <ada@example.com>\", author_time 1600000000 tz 120, committer \"Grace Hopper <grace@example.com>\", committer_time 1600000100 tz -480, creator \"service:importer\", message \"Imported commit\\n\\nWith a body.\\n\", no extensions.",
        obj: Obj::Meta(meta_committer.clone()),
    });
    let mut meta_git = meta_committer.clone();
    meta_git
        .extensions
        .insert(
            META_EXT_GIT_COMMIT,
            hex::decode("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap(),
        )
        .unwrap();
    v.push(Case {
        name: "meta_git_commit_ext",
        description: "meta_with_committer plus extension tag 2 (git commit id, ignorable) with the 20 raw bytes of sha 4b825dc642cb6eb9a060e54bf8d69288fbee4904.",
        obj: Obj::Meta(meta_git),
    });
    v.push(Case {
        name: "meta_negative_time",
        description: "Edge Meta: empty author, author_time seconds -1 tz -720, committer absent, committer_time present (seconds -9223372036854775808, tz 840), empty creator, empty message, no extensions.",
        obj: Obj::Meta(Meta {
            author: Vec::new(),
            author_time: Timestamp {
                seconds: -1,
                tz_offset_minutes: -720,
            },
            committer: None,
            committer_time: Some(Timestamp {
                seconds: i64::MIN,
                tz_offset_minutes: 840,
            }),
            creator: Vec::new(),
            message: Vec::new(),
            extensions: Extensions::new(),
        }),
    });

    // ---- changesets ----
    let root_cid = {
        let mut b = [0u8; 16];
        b.copy_from_slice(&blake3::hash(b"scion-root-v1").as_bytes()[..16]);
        ChangeId(b)
    };
    let cs_root = Changeset {
        parents: Vec::new(),
        root_tree: empty_tree_id(),
        meta: meta_root.id(),
        change_id: root_cid,
        delta: Vec::new(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "changeset_root",
        description: "Root changeset (PLAN 6.5): no parents, root_tree = tree_empty id, meta = meta_root id, change_id = first 16 bytes of BLAKE3(\"scion-root-v1\"), empty delta, no extensions.",
        obj: Obj::Changeset(cs_root.clone()),
    });
    let cs_one = Changeset {
        parents: vec![cs_root.id()],
        root_tree: single.id(),
        meta: meta_native.id(),
        change_id: change_id(0x00),
        delta: vec![DeltaEntry {
            path: path("hello.txt"),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: ObjectId::hash_blob(HELLO),
                size: 6,
            },
        }],
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "changeset_single_parent",
        description: "parents [changeset_root id], root_tree = tree_single_file id, meta = meta_native id, change_id bytes 00 01 .. 0f, delta [Put \"hello.txt\" mode 1 blob(\"hello\\n\") size 6], no extensions.",
        obj: Obj::Changeset(cs_one.clone()),
    });

    let mut subtrees = BTreeMap::new();
    subtrees.insert(subdir.id(), subdir.clone());
    let mut t1 = FlatTree::new();
    flatten(&single, &subtrees, &RepoPath::root(), &mut t1);
    let mut t2 = FlatTree::new();
    flatten(&all_modes, &subtrees, &RepoPath::root(), &mut t2);
    let mixed_delta = file_diff(&t1, &t2);
    let cs_two = Changeset {
        parents: vec![cs_one.id(), cs_root.id()],
        root_tree: all_modes.id(),
        meta: meta_committer.id(),
        change_id: change_id(0xa0),
        delta: mixed_delta,
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "changeset_two_parents_mixed",
        description: "Merge changeset: parents [changeset_single_parent id, changeset_root id], root_tree = tree_all_modes id, meta = meta_with_committer id, change_id bytes a0 a1 .. af, delta = FileDiff(tree_single_file, tree_all_modes) = [Put \"README\" mode 1 blob(\"hello\\n\") 6, Put \"bin/inner.txt\" mode 1 blob(\"inner\\n\") 6, Delete \"hello.txt\", Put \"link\" mode 3 blob(\"../target\") 9, Put \"run.sh\" mode 2 blob(\"#!/bin/sh\\necho hi\\n\") 18], no extensions.",
        obj: Obj::Changeset(cs_two),
    });
    let mut cs_ext = cs_one.clone();
    cs_ext.extensions.insert(8, vec![1, 2, 3]).unwrap();
    v.push(Case {
        name: "changeset_even_extension",
        description: "changeset_single_parent plus one unknown even extension: tag 8, value bytes 01 02 03.",
        obj: Obj::Changeset(cs_ext),
    });

    // ---- history nodes (derived kind 0x10) ----
    let node_root = HistoryNode {
        kind: NodeKind::Dir,
        content: empty_tree_id(),
        changeset: cs_root.id(),
        parents: Vec::new(),
        children: Vec::new(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "history_node_root_dir",
        description: "HistoryNode Dir for the root path of changeset_root: kind 2, content = tree_empty id, changeset = changeset_root id, no parents, no children, no extensions.",
        obj: Obj::Node(node_root.clone()),
    });
    let node_file = HistoryNode {
        kind: NodeKind::File,
        content: ObjectId::hash_blob(HELLO),
        changeset: cs_one.id(),
        parents: Vec::new(),
        children: Vec::new(),
        extensions: Extensions::new(),
    };
    v.push(Case {
        name: "history_node_file",
        description: "HistoryNode File for \"hello.txt\" in changeset_single_parent: kind 1, content = blob id of \"hello\\n\", changeset = changeset_single_parent id, no parents, no children, no extensions.",
        obj: Obj::Node(node_file.clone()),
    });
    v.push(Case {
        name: "history_node_dir",
        description: "HistoryNode Dir for the root path of changeset_single_parent: kind 2, content = tree_single_file id, changeset = changeset_single_parent id, parents [history_node_root_dir id], children [(\"hello.txt\", history_node_file id)], no extensions.",
        obj: Obj::Node(HistoryNode {
            kind: NodeKind::Dir,
            content: single.id(),
            changeset: cs_one.id(),
            parents: vec![node_root.id()],
            children: vec![(name(b"hello.txt"), node_file.id())],
            extensions: Extensions::new(),
        }),
    });
    v
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/golden/objects")
}

fn render(c: &Case) -> String {
    let f = GoldenFile {
        name: c.name.to_owned(),
        kind: c.obj.kind().to_owned(),
        description: c.description.to_owned(),
        hex: hex::encode(c.obj.bytes()),
        id_text: c.obj.id().to_text(),
    };
    let mut s = serde_json::to_string_pretty(&f).unwrap();
    s.push('\n');
    s
}

/// BLAKE3 over `name NUL contents NUL` for every `*.json` file, sorted by file name.
fn digest_of_dir() -> (String, usize) {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(golden_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    files.sort();
    let mut h = blake3::Hasher::new();
    for (n, c) in &files {
        h.update(n.as_bytes());
        h.update(&[0]);
        h.update(c);
        h.update(&[0]);
    }
    (h.finalize().to_hex().to_string(), files.len())
}

#[test]
fn golden_vectors_match_constructors() {
    let cases = cases();
    assert!(cases.len() >= 14);
    let mut names: Vec<&str> = cases.iter().map(|c| c.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), cases.len(), "case names are unique");

    for c in &cases {
        let file = golden_dir().join(format!("{}.json", c.name));
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("missing golden file {}: {e}", file.display()));
        let parsed: GoldenFile = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.name, c.name);
        assert_eq!(parsed.kind, c.obj.kind(), "{}", c.name);
        // Constructor -> bytes and id equal the file.
        assert_eq!(parsed.hex, hex::encode(c.obj.bytes()), "{} bytes", c.name);
        assert_eq!(parsed.id_text, c.obj.id().to_text(), "{} id", c.name);
        assert_eq!(text, render(c), "{} file is canonical", c.name);
        // File -> decode equals the constructor; id from file bytes matches.
        let bytes = hex::decode(&parsed.hex).unwrap();
        c.obj.check_decode(&bytes);
        let id = ObjectId::parse_text(&parsed.id_text).unwrap();
        let kind = match c.obj {
            Obj::Blob(_) => Kind::Blob,
            Obj::Tree(_) => Kind::Tree,
            Obj::Meta(_) => Kind::Meta,
            Obj::Changeset(_) => Kind::Changeset,
            Obj::Node(_) => Kind::Derived(0x10),
        };
        let recomputed = if kind == Kind::Blob {
            ObjectId::hash_blob(&bytes)
        } else {
            ObjectId::hash_encoded(kind, &bytes)
        };
        assert_eq!(id, recomputed, "{} id from bytes", c.name);
    }

    // No stray files.
    let (_, count) = digest_of_dir();
    assert_eq!(
        count,
        cases.len(),
        "golden dir has exactly the constructed cases"
    );
}

#[test]
fn golden_changesets_are_valid() {
    // The changeset vectors satisfy validate_shape, and the merge vector's delta is the
    // FileDiff against its first parent.
    for c in cases() {
        if let Obj::Changeset(cs) = &c.obj {
            cs.validate_shape(&scion_core::Limits::default()).unwrap();
        }
    }
    let cs = cases()
        .into_iter()
        .find(|c| c.name == "changeset_two_parents_mixed")
        .unwrap();
    let Obj::Changeset(cs) = cs.obj else {
        panic!("kind")
    };
    let kinds: Vec<(String, bool)> = cs
        .delta
        .iter()
        .map(|e| (e.path.to_string(), matches!(e.op, DeltaOp::Put { .. })))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("README".into(), true),
            ("bin/inner.txt".into(), true),
            ("hello.txt".into(), false),
            ("link".into(), true),
            ("run.sh".into(), true),
        ]
    );
}

#[test]
fn golden_files_frozen() {
    let (digest, count) = digest_of_dir();
    assert!(count >= 14);
    assert_eq!(
        digest, GOLDEN_DIGEST,
        "testdata/golden/objects changed; golden vectors are frozen (SPEC 19.1)"
    );
}

#[test]
#[ignore]
fn regenerate_golden_vectors() {
    if std::env::var("SCION_REGEN_GOLDEN").as_deref() != Ok("1") {
        eprintln!("SKIP: set SCION_REGEN_GOLDEN=1 to rewrite testdata/golden/objects");
        return;
    }
    let dir = golden_dir();
    std::fs::create_dir_all(&dir).unwrap();
    for c in cases() {
        std::fs::write(dir.join(format!("{}.json", c.name)), render(&c)).unwrap();
    }
    let (digest, count) = digest_of_dir();
    eprintln!("golden digest over {count} files: {digest}");
}
