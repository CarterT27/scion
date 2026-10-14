//! SPEC 19.1 decoder rejections (one test per class), unknown even extensions preserved
//! byte for byte, and kind confusion (equal digests, different kinds).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::{ChangeId, ErrorCode, Kind, Mode, ObjectId, Result, empty_tree_id, subcode};
use scion_object::enc::{Decoder, Encoder};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, HistoryNode, Meta, Name, NodeKind,
    RepoPath, Timestamp, Tree, TreeEntry,
};

fn assert_sub<T: std::fmt::Debug>(r: Result<T>, want: &str) {
    let e = r.expect_err("expected rejection");
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    assert_eq!(e.subcode(), Some(want), "{e}");
}

fn blob(s: &[u8]) -> ObjectId {
    ObjectId::hash_blob(s)
}

/// Hand-built tree bytes: entries `(name, mode byte, id, size)`, then raw extension pairs.
fn tree_bytes(
    version: u64,
    entries: &[(&[u8], u8, ObjectId, u64)],
    ext: &[(u64, &[u8])],
) -> Vec<u8> {
    let mut e = Encoder::new();
    e.uvarint(version);
    e.uvarint(entries.len() as u64);
    for (n, m, id, size) in entries {
        e.bytes(n);
        e.u8(*m);
        e.id(id);
        e.uvarint(*size);
    }
    e.uvarint(ext.len() as u64);
    for (t, v) in ext {
        e.uvarint(*t);
        e.bytes(v);
    }
    e.finish()
}

fn one_file_tree() -> Tree {
    Tree {
        entries: vec![TreeEntry {
            name: Name::new(b"a").unwrap(),
            mode: Mode::File,
            id: blob(b"a"),
            size: 1,
        }],
        extensions: Extensions::new(),
    }
}

#[test]
fn reject_non_minimal_varint() {
    // format_version 1 encoded as 0x81 0x00.
    assert_sub(
        Tree::decode(&[0x81, 0x00, 0x00, 0x00]),
        subcode::NON_MINIMAL_VARINT,
    );
    // Entry count 0 encoded as 0x80 0x00.
    assert_sub(
        Tree::decode(&[0x01, 0x80, 0x00, 0x00]),
        subcode::NON_MINIMAL_VARINT,
    );
    // A size field with a redundant zero group.
    let mut b = tree_bytes(1, &[(b"a", 1, blob(b"a"), 1)], &[]);
    let size_at = b.len() - 2;
    assert_eq!(b[size_at], 0x01);
    b.splice(size_at..=size_at, [0x81, 0x00]);
    assert_sub(Tree::decode(&b), subcode::NON_MINIMAL_VARINT);
}

#[test]
fn reject_eleven_byte_varint() {
    let mut b = vec![0x01, 0x01, 0x01, b'a', 0x01];
    b.extend_from_slice(blob(b"a").as_bytes());
    b.extend_from_slice(&[0xff; 10]);
    b.push(0x01);
    b.push(0x00);
    assert_sub(Tree::decode(&b), subcode::VARINT_OVERFLOW);
    let mut d = Decoder::new(&[
        0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
    ]);
    assert_sub(d.uvarint(), subcode::VARINT_OVERFLOW);
}

#[test]
fn reject_length_overrun() {
    // Name length 5 with 1 byte left.
    assert_sub(
        Tree::decode(&[0x01, 0x01, 0x05, b'a']),
        subcode::LENGTH_OVERRUN,
    );
    // Entry count larger than the remaining input.
    assert_sub(Tree::decode(&[0x01, 0x7f, 0x00]), subcode::LENGTH_OVERRUN);
    // Truncated id.
    let b = one_file_tree().encode();
    assert_sub(Tree::decode(&b[..10]), subcode::LENGTH_OVERRUN);
    // Meta message length beyond input.
    let mut m = Meta {
        author: vec![],
        author_time: Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: vec![],
        message: b"hello".to_vec(),
        extensions: Extensions::new(),
    }
    .encode();
    m.truncate(m.len() - 3);
    assert_sub(Meta::decode(&m), subcode::LENGTH_OVERRUN);
}

#[test]
fn reject_trailing_bytes() {
    let mut b = one_file_tree().encode();
    b.push(0x00);
    assert_sub(Tree::decode(&b), subcode::TRAILING_BYTES);
    assert_sub(
        Tree::decode(&[0x01, 0x00, 0x00, 0x00]),
        subcode::TRAILING_BYTES,
    );
}

#[test]
fn reject_unsorted_entries() {
    let b = tree_bytes(
        1,
        &[(b"b", 1, blob(b"b"), 1), (b"a", 1, blob(b"a"), 1)],
        &[],
    );
    assert_sub(Tree::decode(&b), subcode::UNSORTED);
    // Bytewise, not case-insensitive or UTF-8 order: "a" (0x61) before "B" (0x42) is unsorted.
    let b = tree_bytes(
        1,
        &[(b"a", 1, blob(b"a"), 1), (b"B", 1, blob(b"B"), 1)],
        &[],
    );
    assert_sub(Tree::decode(&b), subcode::UNSORTED);
    // Unsorted extension tags.
    let b = tree_bytes(1, &[], &[(4, b""), (2, b"")]);
    assert_sub(Tree::decode(&b), subcode::UNSORTED);
    // Unsorted delta paths in a changeset.
    let mut cs = changeset();
    cs.delta.reverse();
    assert_sub(Changeset::decode(&cs.encode()), subcode::UNSORTED);
}

#[test]
fn reject_duplicate_entries() {
    let b = tree_bytes(
        1,
        &[(b"a", 1, blob(b"a"), 1), (b"a", 2, blob(b"a"), 1)],
        &[],
    );
    assert_sub(Tree::decode(&b), subcode::DUPLICATE);
    let b = tree_bytes(1, &[], &[(2, b"x"), (2, b"y")]);
    assert_sub(Tree::decode(&b), subcode::DUPLICATE);
    let mut cs = changeset();
    cs.parents = vec![cs.parents[0], cs.parents[0]];
    assert_sub(Changeset::decode(&cs.encode()), subcode::DUPLICATE);
    let mut cs = changeset();
    cs.delta[1].path = cs.delta[0].path.clone();
    assert_sub(Changeset::decode(&cs.encode()), subcode::DUPLICATE);
}

#[test]
fn reject_unknown_format_version() {
    assert_sub(
        Tree::decode(&[0x02, 0x00, 0x00]),
        subcode::UNKNOWN_FORMAT_VERSION,
    );
    assert_sub(
        Tree::decode(&[0x00, 0x00, 0x00]),
        subcode::UNKNOWN_FORMAT_VERSION,
    );
    for mut b in [meta().encode(), changeset().encode(), node().encode()] {
        b[0] = 0x02;
        let r = (
            Meta::decode(&b).err(),
            Changeset::decode(&b).err(),
            HistoryNode::decode(&b).err(),
        );
        for e in [r.0, r.1, r.2] {
            assert_eq!(e.unwrap().subcode(), Some(subcode::UNKNOWN_FORMAT_VERSION));
        }
    }
}

#[test]
fn reject_unknown_odd_extension_tag() {
    let b = tree_bytes(1, &[], &[(1, b"crit")]);
    assert_sub(Tree::decode(&b), subcode::UNKNOWN_CRITICAL_EXTENSION);
    let b = tree_bytes(1, &[], &[(2, b""), (3, b"")]);
    assert_sub(Tree::decode(&b), subcode::UNKNOWN_CRITICAL_EXTENSION);
    // Same for every structured kind: replace the trailing empty extension list.
    for (enc, which) in [
        (meta().encode(), 0),
        (changeset().encode(), 1),
        (node().encode(), 2),
    ] {
        let mut b = enc;
        assert_eq!(b.pop(), Some(0x00));
        b.extend_from_slice(&[0x01, 0x05, 0x00]);
        let e = match which {
            0 => Meta::decode(&b).unwrap_err(),
            1 => Changeset::decode(&b).unwrap_err(),
            _ => HistoryNode::decode(&b).unwrap_err(),
        };
        assert_eq!(e.subcode(), Some(subcode::UNKNOWN_CRITICAL_EXTENSION));
    }
    // Constructing one is refused too.
    assert_sub(
        Extensions::new().insert(7, vec![]),
        subcode::UNKNOWN_CRITICAL_EXTENSION,
    );
}

#[test]
fn reject_invalid_mode() {
    for m in [0u8, 5, 0xff] {
        let b = tree_bytes(1, &[(b"a", m, blob(b"a"), 1)], &[]);
        assert_sub(Tree::decode(&b), subcode::INVALID_MODE);
    }
    // Mode 4 with a non-zero size.
    let b = tree_bytes(1, &[(b"d", 4, empty_tree_id(), 3)], &[]);
    assert_sub(Tree::decode(&b), subcode::INVALID_MODE);
    // Delta Put with mode 4.
    let mut cs = changeset();
    cs.delta[0].op = DeltaOp::Put {
        mode: Mode::Tree,
        id: blob(b"x"),
        size: 0,
    };
    assert_sub(Changeset::decode(&cs.encode()), subcode::INVALID_MODE);
}

#[test]
fn reject_mode_kind_mismatch() {
    // Mode 1-3 with a Tree id.
    for m in [1u8, 2, 3] {
        let b = tree_bytes(1, &[(b"a", m, empty_tree_id(), 0)], &[]);
        assert_sub(Tree::decode(&b), subcode::INVALID_KIND);
    }
    // Mode 4 with a Blob id.
    let b = tree_bytes(1, &[(b"d", 4, blob(b"a"), 0)], &[]);
    assert_sub(Tree::decode(&b), subcode::INVALID_KIND);
}

#[test]
fn reject_invalid_names_in_trees() {
    for n in [&b"."[..], b"..", b"a/b", b"a\0", b".scion", b".SCION", b""] {
        let b = tree_bytes(1, &[(n, 1, blob(b"a"), 1)], &[]);
        assert_sub(Tree::decode(&b), subcode::INVALID_PATH);
    }
    let long = vec![b'n'; 256];
    let b = tree_bytes(1, &[(&long, 1, blob(b"a"), 1)], &[]);
    assert_sub(Tree::decode(&b), subcode::INVALID_PATH);
}

#[test]
fn reject_invalid_option_tag_and_delta_op() {
    let mut b = meta().encode();
    // Layout: fv, author(len 1 'a'), ts(2 bytes), committer tag at index 5.
    assert_eq!(b[5], 0x00);
    b[5] = 0x02;
    assert_sub(Meta::decode(&b), subcode::INVALID_KIND);

    let cs = changeset();
    let mut b = cs.encode();
    // Find the Delete op byte: the second delta entry is "b" Delete.
    let pos = b.windows(3).rposition(|w| w == [0x01, b'b', 0x02]).unwrap();
    b[pos + 2] = 0x03;
    assert_sub(Changeset::decode(&b), subcode::INVALID_MODE);
}

#[test]
fn unknown_even_tag_preserved_and_changes_id() {
    let plain = one_file_tree();
    let b = tree_bytes(
        1,
        &[(b"a", 1, blob(b"a"), 1)],
        &[(10, b"\x00\xffopaque"), (1 << 40, b"")],
    );
    let t = Tree::decode(&b).unwrap();
    assert_eq!(t.extensions.get(10), Some(&b"\x00\xffopaque"[..]));
    assert_eq!(t.extensions.get(1 << 40), Some(&b""[..]));
    assert_eq!(t.encode(), b, "preserved byte for byte");
    assert_eq!(t.entries, plain.entries);
    assert_ne!(t.id(), plain.id(), "extension changes the id");
    assert_eq!(t.id(), ObjectId::hash_encoded(Kind::Tree, &b));

    let mut m = meta();
    let base = m.id();
    m.extensions.insert(6, b"x".to_vec()).unwrap();
    let back = Meta::decode(&m.encode()).unwrap();
    assert_eq!(back, m);
    assert_ne!(back.id(), base);

    let mut c = changeset();
    let base = c.id();
    c.extensions.insert(2, vec![]).unwrap();
    assert_eq!(Changeset::decode(&c.encode()).unwrap(), c);
    assert_ne!(c.id(), base, "even an empty value changes the id");

    let mut n = node();
    let base = n.id();
    n.extensions.insert(4, vec![1]).unwrap();
    assert_eq!(HistoryNode::decode(&n.encode()).unwrap(), n);
    assert_ne!(n.id(), base);
}

#[test]
fn equal_digests_different_kinds_are_distinct() {
    let digest = *blob(b"a").digest();
    let ids: Vec<ObjectId> = [
        Kind::Blob,
        Kind::Tree,
        Kind::Changeset,
        Kind::Meta,
        Kind::Derived(0x10),
    ]
    .into_iter()
    .map(|k| ObjectId::new(k, digest))
    .collect();
    for (i, a) in ids.iter().enumerate() {
        for (j, b) in ids.iter().enumerate() {
            assert_eq!(i == j, a == b);
            assert_eq!(i == j, a.to_text() == b.to_text());
        }
        assert_eq!(ObjectId::parse_text(&a.to_text()).unwrap(), *a);
    }
    let (blob_id, tree_id, cs_id, meta_id, node_id) = (ids[0], ids[1], ids[2], ids[3], ids[4]);

    // A Tree id where a Blob id is required (tree entry modes 1-3, delta Put).
    let b = tree_bytes(1, &[(b"a", 1, tree_id, 1)], &[]);
    assert_sub(Tree::decode(&b), subcode::INVALID_KIND);
    let mut cs = changeset();
    cs.delta[0].op = DeltaOp::Put {
        mode: Mode::File,
        id: tree_id,
        size: 1,
    };
    assert_sub(Changeset::decode(&cs.encode()), subcode::INVALID_KIND);
    assert_sub(
        cs.validate_shape(&scion_core::Limits::default()),
        subcode::INVALID_KIND,
    );
    // A Blob id where a Tree id is required (mode 4, root_tree).
    let b = tree_bytes(1, &[(b"d", 4, blob_id, 0)], &[]);
    assert_sub(Tree::decode(&b), subcode::INVALID_KIND);
    let mut cs = changeset();
    cs.root_tree = blob_id;
    assert_sub(Changeset::decode(&cs.encode()), subcode::INVALID_KIND);
    // Meta / Changeset confusion in a changeset.
    let mut cs = changeset();
    cs.meta = cs_id;
    assert_sub(Changeset::decode(&cs.encode()), subcode::INVALID_KIND);
    let mut cs = changeset();
    cs.parents = vec![meta_id];
    assert_sub(Changeset::decode(&cs.encode()), subcode::INVALID_KIND);
    // History nodes: parents/children must be node ids, changeset a Changeset id.
    let mut n = node();
    n.parents = vec![cs_id];
    assert_sub(HistoryNode::decode(&n.encode()), subcode::INVALID_KIND);
    let mut n = node();
    n.changeset = node_id;
    assert_sub(HistoryNode::decode(&n.encode()), subcode::INVALID_KIND);
    let mut n = node();
    n.content = blob_id;
    assert_sub(HistoryNode::decode(&n.encode()), subcode::INVALID_KIND);
}

#[test]
fn bad_algo_and_kind_bytes_in_ids() {
    let mut b = one_file_tree().encode();
    // Entry: fv, count, name len, 'a', mode, then the id.
    let id_at = 5;
    assert_eq!(b[id_at], 0x01);
    b[id_at] = 0x02;
    assert_sub(Tree::decode(&b), subcode::INVALID_ALGO);
    b[id_at] = 0x01;
    b[id_at + 1] = 0x09;
    assert_sub(Tree::decode(&b), subcode::INVALID_KIND);
}

#[test]
fn empty_non_root_tree() {
    assert_sub(Tree::empty().validate_non_root(), subcode::EMPTY_TREE);
    one_file_tree().validate_non_root().unwrap();
}

fn meta() -> Meta {
    Meta {
        author: b"a".to_vec(),
        author_time: Timestamp {
            seconds: 1,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"github:a".to_vec(),
        message: b"m".to_vec(),
        extensions: Extensions::new(),
    }
}

fn changeset() -> Changeset {
    Changeset {
        parents: vec![ObjectId::new(Kind::Changeset, [1; 32])],
        root_tree: one_file_tree().id(),
        meta: meta().id(),
        change_id: ChangeId([7; 16]),
        delta: vec![
            DeltaEntry {
                path: RepoPath::parse(b"a").unwrap(),
                op: DeltaOp::Put {
                    mode: Mode::File,
                    id: blob(b"a"),
                    size: 1,
                },
            },
            DeltaEntry {
                path: RepoPath::parse(b"b").unwrap(),
                op: DeltaOp::Delete,
            },
        ],
        extensions: Extensions::new(),
    }
}

fn node() -> HistoryNode {
    HistoryNode {
        kind: NodeKind::Dir,
        content: one_file_tree().id(),
        changeset: changeset().id(),
        parents: vec![ObjectId::new(Kind::Derived(0x10), [2; 32])],
        children: vec![(
            Name::new(b"a").unwrap(),
            ObjectId::new(Kind::Derived(0x10), [3; 32]),
        )],
        extensions: Extensions::new(),
    }
}

#[test]
fn delta_paths_validated_on_decode() {
    for bad in [
        &b""[..],
        b"/a",
        b"a/",
        b"a//b",
        b"./a",
        b"a/../b",
        b".scion/x",
        b"x/.Scion",
    ] {
        let mut e = Encoder::new();
        e.uvarint(1);
        e.uvarint(0);
        e.id(&empty_tree_id());
        e.id(&meta().id());
        e.raw(&[0; 16]);
        e.uvarint(1);
        e.bytes(bad);
        e.u8(2);
        e.uvarint(0);
        assert_sub(Changeset::decode(&e.finish()), subcode::INVALID_PATH);
    }
}
