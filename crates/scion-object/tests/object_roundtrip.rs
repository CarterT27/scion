//! SPEC 19.1 round trips: `decode(encode(x)) == x` and `encode(decode(b)) == b` for random
//! valid Tree / Meta / Changeset / HistoryNode values, plus a canonicality property: any
//! mutation of a valid encoding either fails to decode or re-encodes to exactly the mutated
//! bytes (one byte string per logical value, SPEC 4.1).
//!
//! Default runs use `proptest_cases(10_000)` (PLAN 3.5); `full_roundtrip_1e6` runs the SPEC
//! count of 10^6 under `--include-ignored` (scripts/check.sh --full sets SCION_FULL=1).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::collection::{btree_map, btree_set, vec};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use scion_core::test_support::{full_mode, proptest_cases};
use scion_core::{ChangeId, Kind, Limits, Mode, ObjectId};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, HistoryNode, Meta, Name, NodeKind,
    RepoPath, Timestamp, Tree, TreeEntry,
};

fn arb_id(kind: Kind) -> impl Strategy<Value = ObjectId> {
    any::<[u8; 32]>().prop_map(move |d| ObjectId::new(kind, d))
}

fn arb_name_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Mostly short ASCII names, so collisions and ordering are exercised.
        vec(prop::sample::select(b"abcAB._-0".to_vec()), 1..6),
        // Arbitrary bytes except NUL and '/'.
        vec(
            any::<u8>().prop_filter("no NUL or slash", |b| *b != 0 && *b != b'/'),
            1..20
        ),
    ]
    .prop_filter("valid SPEC 4.4 name", |b| Name::new(b).is_ok())
}

fn arb_u64() -> impl Strategy<Value = u64> {
    prop_oneof![0u64..300, any::<u64>()]
}

fn arb_extensions() -> impl Strategy<Value = Extensions> {
    btree_map(
        prop_oneof![0u64..40, any::<u64>()].prop_map(|t| t & !1),
        vec(any::<u8>(), 0..12),
        0..3,
    )
    .prop_map(|m| {
        let mut x = Extensions::new();
        for (t, v) in m {
            x.insert(t, v).unwrap();
        }
        x
    })
}

fn arb_mode() -> impl Strategy<Value = Mode> {
    prop::sample::select(vec![
        Mode::File,
        Mode::Executable,
        Mode::Symlink,
        Mode::Tree,
    ])
}

fn arb_tree() -> impl Strategy<Value = Tree> {
    (
        btree_map(
            arb_name_bytes(),
            (arb_mode(), any::<[u8; 32]>(), arb_u64()),
            0..12,
        ),
        arb_extensions(),
    )
        .prop_map(|(m, extensions)| Tree {
            entries: m
                .into_iter()
                .map(|(name, (mode, digest, size))| {
                    let tree = mode == Mode::Tree;
                    TreeEntry {
                        name: Name::new(&name).unwrap(),
                        mode,
                        id: ObjectId::new(if tree { Kind::Tree } else { Kind::Blob }, digest),
                        size: if tree { 0 } else { size },
                    }
                })
                .collect(),
            extensions,
        })
}

fn arb_ts() -> impl Strategy<Value = Timestamp> {
    (
        prop_oneof![-100i64..100, any::<i64>()],
        prop_oneof![-840i64..=840, any::<i64>()],
    )
        .prop_map(|(seconds, tz_offset_minutes)| Timestamp {
            seconds,
            tz_offset_minutes,
        })
}

fn arb_meta() -> impl Strategy<Value = Meta> {
    (
        vec(any::<u8>(), 0..40),
        arb_ts(),
        proptest::option::of(vec(any::<u8>(), 0..40)),
        proptest::option::of(arb_ts()),
        vec(any::<u8>(), 0..20),
        vec(any::<u8>(), 0..200),
        arb_extensions(),
    )
        .prop_map(
            |(author, author_time, committer, committer_time, creator, message, extensions)| Meta {
                author,
                author_time,
                committer,
                committer_time,
                creator,
                message,
                extensions,
            },
        )
}

fn arb_path() -> impl Strategy<Value = RepoPath> {
    vec(arb_name_bytes(), 1..4).prop_map(|names| RepoPath::parse(&names.join(&b'/')).unwrap())
}

fn arb_op() -> impl Strategy<Value = DeltaOp> {
    prop_oneof![
        Just(DeltaOp::Delete),
        (
            prop::sample::select(vec![Mode::File, Mode::Executable, Mode::Symlink]),
            arb_id(Kind::Blob),
            arb_u64()
        )
            .prop_map(|(mode, id, size)| DeltaOp::Put { mode, id, size }),
    ]
}

fn arb_changeset() -> impl Strategy<Value = Changeset> {
    (
        vec(any::<[u8; 32]>(), 0..4),
        arb_id(Kind::Tree),
        arb_id(Kind::Meta),
        any::<[u8; 16]>(),
        btree_map(arb_path(), arb_op(), 0..10),
        arb_extensions(),
    )
        .prop_map(|(ps, root_tree, meta, cid, delta, extensions)| {
            let mut parents: Vec<ObjectId> = Vec::new();
            for d in ps {
                let id = ObjectId::new(Kind::Changeset, d);
                if !parents.contains(&id) {
                    parents.push(id);
                }
            }
            Changeset {
                parents,
                root_tree,
                meta,
                change_id: ChangeId(cid),
                delta: delta
                    .into_iter()
                    .map(|(path, op)| DeltaEntry { path, op })
                    .collect(),
                extensions,
            }
        })
}

fn arb_node() -> impl Strategy<Value = HistoryNode> {
    (
        any::<bool>(),
        any::<[u8; 32]>(),
        arb_id(Kind::Changeset),
        btree_set(any::<[u8; 32]>(), 0..3),
        btree_map(arb_name_bytes(), any::<[u8; 32]>(), 0..6),
        arb_extensions(),
    )
        .prop_map(|(dir, content, changeset, parents, children, extensions)| {
            let node = Kind::Derived(0x10);
            HistoryNode {
                kind: if dir { NodeKind::Dir } else { NodeKind::File },
                content: ObjectId::new(if dir { Kind::Tree } else { Kind::Blob }, content),
                changeset,
                parents: parents
                    .into_iter()
                    .map(|d| ObjectId::new(node, d))
                    .collect(),
                children: if dir {
                    children
                        .into_iter()
                        .map(|(n, d)| (Name::new(&n).unwrap(), ObjectId::new(node, d)))
                        .collect()
                } else {
                    Vec::new()
                },
                extensions,
            }
        })
}

fn round_trip<T: Canonical + PartialEq + std::fmt::Debug>(x: &T) -> Result<(), TestCaseError> {
    let b = x.encode();
    let back = T::decode(&b).map_err(|e| TestCaseError::fail(format!("decode failed: {e}")))?;
    prop_assert_eq!(&back, x);
    prop_assert_eq!(back.encode(), b);
    prop_assert_eq!(back.id(), x.id());
    Ok(())
}

/// Mutations of a valid encoding: overwrite, insert or delete one byte, or truncate.
#[derive(Clone, Debug)]
enum Mutation {
    Set(usize, u8),
    Insert(usize, u8),
    Delete(usize),
    Truncate(usize),
}

fn arb_mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Mutation::Set(i, b)),
        (any::<usize>(), any::<u8>()).prop_map(|(i, b)| Mutation::Insert(i, b)),
        any::<usize>().prop_map(Mutation::Delete),
        any::<usize>().prop_map(Mutation::Truncate),
    ]
}

fn mutate(mut b: Vec<u8>, m: &Mutation) -> Vec<u8> {
    let n = b.len();
    match *m {
        Mutation::Set(i, v) if n > 0 => b[i % n] = v,
        Mutation::Insert(i, v) => b.insert(i % (n + 1), v),
        Mutation::Delete(i) if n > 0 => {
            b.remove(i % n);
        }
        Mutation::Truncate(i) => b.truncate(i % (n + 1)),
        _ => {}
    }
    b
}

/// Canonicality: a decodable byte string re-encodes to itself.
fn canonical_after_mutation<T: Canonical>(x: &T, m: &Mutation) -> Result<(), TestCaseError> {
    let b = mutate(x.encode(), m);
    if let Ok(v) = T::decode(&b) {
        prop_assert_eq!(v.encode(), b);
    }
    Ok(())
}

fn run<S: Strategy>(cases: u32, s: S, f: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    });
    runner.run(&s, f).unwrap();
}

fn all_round_trips(cases: u32) {
    run(cases, arb_tree(), |t| {
        prop_assert!(t.validate().is_ok());
        round_trip(&t)
    });
    run(cases, arb_meta(), |m| round_trip(&m));
    run(cases, arb_changeset(), |c| {
        prop_assert!(c.validate_shape(&Limits::default()).is_ok());
        round_trip(&c)
    });
    run(cases, arb_node(), |n| round_trip(&n));
}

fn all_mutations(cases: u32) {
    run(cases, (arb_tree(), arb_mutation()), |(x, m)| {
        canonical_after_mutation(&x, &m)
    });
    run(cases, (arb_meta(), arb_mutation()), |(x, m)| {
        canonical_after_mutation(&x, &m)
    });
    run(cases, (arb_changeset(), arb_mutation()), |(x, m)| {
        canonical_after_mutation(&x, &m)
    });
    run(cases, (arb_node(), arb_mutation()), |(x, m)| {
        canonical_after_mutation(&x, &m)
    });
}

#[test]
fn round_trip_random_valid_objects() {
    all_round_trips(proptest_cases(10_000));
}

#[test]
fn mutated_encodings_are_rejected_or_canonical() {
    all_mutations(proptest_cases(10_000));
}

#[test]
fn random_bytes_never_panic_and_stay_canonical() {
    run(proptest_cases(10_000), vec(any::<u8>(), 0..120), |b| {
        if let Ok(t) = Tree::decode(&b) {
            prop_assert_eq!(t.encode(), b.clone());
        }
        if let Ok(m) = Meta::decode(&b) {
            prop_assert_eq!(m.encode(), b.clone());
        }
        if let Ok(c) = Changeset::decode(&b) {
            prop_assert_eq!(c.encode(), b.clone());
        }
        if let Ok(n) = HistoryNode::decode(&b) {
            prop_assert_eq!(n.encode(), b);
        }
        Ok(())
    });
}

#[test]
#[ignore]
fn full_roundtrip_1e6() {
    if !full_mode() {
        eprintln!("note: SCION_FULL is not set; running the spec-sized count anyway");
    }
    all_round_trips(1_000_000);
    all_mutations(1_000_000);
}
