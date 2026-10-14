//! SPEC 19.7: conversions between `v1` messages and domain types: property round trips,
//! forward compatibility (unknown fields are ignored) and missing required fields
//! (INVALID_ARGUMENT/missing_field).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod proto_support;

use prost::Message;
use proto_support::generators::{self as g, Rng};
use scion_core::test_support::proptest_cases;
use scion_core::{ConflictReport, EntryInfo, ErrorCode, Limits, Result, subcode};
use scion_object::{Extensions, Timestamp};
use scion_proto::convert::{
    self, CreateChangesetParts, MergeParts, MetaFieldsParts, id_bytes, payload_from_proto,
    payload_to_proto,
};
use scion_proto::v1;

/// Encode, decode (as a peer would), and return the decoded message.
fn wire<M: Message + Default>(m: &M) -> M {
    M::decode(m.encode_to_vec().as_slice()).unwrap()
}

fn meta(r: &mut Rng) -> MetaFieldsParts {
    let committer = r.bool().then(|| r.bytes(20));
    MetaFieldsParts {
        author: r.bytes(30),
        author_time: g::timestamp(r),
        committer_time: committer.as_ref().map(|_| g::timestamp(r)),
        committer,
        message: r.bytes(60),
        // Non-empty extensions need Extensions::insert (scion-object, P01); see the ignored test.
        extensions: Extensions::new(),
    }
}

fn round_trips(cases: u32, seed: u64) {
    let mut r = Rng::new(seed);
    for _ in 0..cases {
        // Ids.
        let id = g::oid(&mut r);
        assert_eq!(convert::required_id(&id_bytes(&id), "id").unwrap(), id);

        // Entries.
        let e = g::entry(&mut r);
        assert_eq!(
            EntryInfo::try_from(&wire(&v1::EntryInfo::from(&e))).unwrap(),
            e
        );

        // Timestamps.
        let t = g::timestamp(&mut r);
        assert_eq!(Timestamp::from(&wire(&v1::Timestamp::from(&t))), t);

        // Limits.
        let l = g::limits(&mut r);
        assert_eq!(Limits::from(&wire(&v1::LimitsMsg::from(&l))), l);

        // Conflict reports.
        let c = g::conflict_report(&mut r);
        assert_eq!(
            ConflictReport::try_from(&wire(&v1::ConflictReport::from(&c))).unwrap(),
            c
        );

        // Error payloads.
        let p = g::payload(&mut r);
        let m = payload_to_proto(&p).map(|m| wire(&m));
        assert_eq!(payload_from_proto(m.as_ref()).unwrap(), p);

        // Error details.
        let err = g::error(&mut r);
        let d = wire(&v1::ErrorDetail::from(&err));
        assert_eq!(scion_core::ScionError::try_from(&d).unwrap(), err);

        // Public keys.
        let mut key = [0u8; 32];
        key.iter_mut().for_each(|b| *b = r.next() as u8);
        let kid = format!("{:016x}", r.next());
        let pk = wire(&convert::public_key_to_proto(&kid, &key));
        assert_eq!(convert::public_key_from_proto(&pk).unwrap(), (kid, key));

        // Write requests (empty delta: delta paths need RepoPath::parse, P01).
        let create = CreateChangesetParts {
            parents: (0..r.below(4)).map(|_| g::oid(&mut r)).collect(),
            delta: Vec::new(),
            meta: meta(&mut r),
            change_id: g::change_id(&mut r),
            expected_root_tree: r.bool().then(|| g::oid(&mut r)),
            import_mode: r.bool(),
        };
        let m = wire(&v1::CreateChangesetRequest::from(&create));
        assert_eq!(CreateChangesetParts::try_from(&m).unwrap(), create);

        let merge = MergeParts {
            a: g::oid(&mut r),
            b: g::oid(&mut r),
            meta: meta(&mut r),
            change_id: g::change_id(&mut r),
        };
        let m = wire(&v1::MergeRequest::from(&merge));
        assert_eq!(MergeParts::try_from(&m).unwrap(), merge);
    }
}

#[test]
fn property_round_trip() {
    round_trips(proptest_cases(2_000), 0x5c10);
}

#[test]
#[ignore = "spec-sized; run with scripts/check.sh --full"]
fn full_property_round_trip() {
    let n = if scion_core::test_support::full_mode() {
        1_000_000
    } else {
        proptest_cases(10_000)
    };
    round_trips(n, 0x5c11);
}

fn sample_create() -> v1::CreateChangesetRequest {
    let mut r = Rng::new(42);
    v1::CreateChangesetRequest::from(&CreateChangesetParts {
        parents: vec![g::oid(&mut r)],
        delta: Vec::new(),
        meta: meta(&mut r),
        change_id: g::change_id(&mut r),
        expected_root_tree: None,
        import_mode: false,
    })
}

/// Append an unknown varint field and an unknown length-delimited field.
fn with_unknown_fields(mut b: Vec<u8>) -> Vec<u8> {
    // field 100, wire type 0 (varint): key = 100 << 3 = 800 = 0xa0 0x06; value 42.
    b.extend_from_slice(&[0xa0, 0x06, 0x2a]);
    // field 101, wire type 2 (len): key = (101 << 3) | 2 = 810 = 0xaa 0x06; 3 bytes.
    b.extend_from_slice(&[0xaa, 0x06, 0x03, b'x', b'y', b'z']);
    // field 15 (unused in this message), fixed64.
    b.extend_from_slice(&[0x79, 1, 2, 3, 4, 5, 6, 7, 8]);
    b
}

#[test]
fn unknown_request_fields_are_ignored() {
    let m = sample_create();
    let decoded =
        v1::CreateChangesetRequest::decode(with_unknown_fields(m.encode_to_vec()).as_slice())
            .unwrap();
    assert_eq!(decoded, m);
    assert!(CreateChangesetParts::try_from(&decoded).is_ok());

    // Unknown fields inside a nested message (MetaFields) as well.
    let mut meta_bytes = m.meta_fields.clone().unwrap().encode_to_vec();
    meta_bytes = with_unknown_fields(meta_bytes);
    let mut outer = v1::CreateChangesetRequest {
        meta_fields: None,
        ..m.clone()
    }
    .encode_to_vec();
    // field 3 (meta_fields), wire type 2.
    outer.push(0x1a);
    let mut len = meta_bytes.len();
    while len >= 0x80 {
        outer.push((len as u8 & 0x7f) | 0x80);
        len >>= 7;
    }
    outer.push(len as u8);
    outer.extend_from_slice(&meta_bytes);
    let decoded = v1::CreateChangesetRequest::decode(outer.as_slice()).unwrap();
    assert_eq!(decoded, m);

    // Other request types.
    let g = v1::GetObjectsRequest {
        ids: vec![vec![1; 34]],
        offset: Some(1),
        length: Some(2),
    };
    assert_eq!(
        v1::GetObjectsRequest::decode(with_unknown_fields(g.encode_to_vec()).as_slice()).unwrap(),
        g
    );
    let e = v1::GetServerInfoRequest {};
    assert_eq!(
        v1::GetServerInfoRequest::decode(with_unknown_fields(Vec::new()).as_slice()).unwrap(),
        e
    );
    let d = v1::ErrorDetail {
        code: "NOT_FOUND".into(),
        retryable: false,
        message: "m".into(),
        payload: None,
    };
    assert_eq!(
        v1::ErrorDetail::decode(with_unknown_fields(d.encode_to_vec()).as_slice()).unwrap(),
        d
    );
}

fn assert_missing<T: std::fmt::Debug>(r: Result<T>, what: &str) {
    let e = r.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{what}");
    assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD), "{what}: {e}");
}

#[test]
fn create_changeset_missing_required_fields() {
    let ok = sample_create();
    assert!(CreateChangesetParts::try_from(&ok).is_ok());

    let m = v1::CreateChangesetRequest {
        meta_fields: None,
        ..ok.clone()
    };
    assert_missing(CreateChangesetParts::try_from(&m), "meta_fields");

    let m = v1::CreateChangesetRequest {
        change_id: Vec::new(),
        ..ok.clone()
    };
    assert_missing(CreateChangesetParts::try_from(&m), "change_id");

    let mut m = ok.clone();
    m.meta_fields.as_mut().unwrap().author_time = None;
    assert_missing(CreateChangesetParts::try_from(&m), "author_time");

    let m = v1::CreateChangesetRequest {
        expected_root_tree: Some(Vec::new()),
        ..ok.clone()
    };
    assert_missing(CreateChangesetParts::try_from(&m), "expected_root_tree");

    // A completely empty request (all defaults) is missing its required fields.
    assert_missing(
        CreateChangesetParts::try_from(&v1::CreateChangesetRequest::default()),
        "empty",
    );

    // Malformed ids are bad_id_text.
    for m in [
        v1::CreateChangesetRequest {
            change_id: vec![1; 15],
            ..ok.clone()
        },
        v1::CreateChangesetRequest {
            parents: vec![vec![9; 34]],
            ..ok.clone()
        },
        v1::CreateChangesetRequest {
            expected_root_tree: Some(vec![1, 2, 3]),
            ..ok.clone()
        },
    ] {
        let e = CreateChangesetParts::try_from(&m).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT), "{e}");
    }

    // Delta entries with missing op / id are missing_field (checked before path parsing).
    for d in [
        v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: 0,
            ..Default::default()
        },
        v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: convert::DELTA_OP_PUT,
            mode: 1,
            id: Vec::new(),
            size: 1,
        },
        v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: convert::DELTA_OP_PUT,
            mode: 0,
            id: id_bytes(&scion_core::ObjectId::hash_blob(b"a")),
            size: 1,
        },
    ] {
        let m = v1::CreateChangesetRequest {
            delta: vec![d],
            ..ok.clone()
        };
        assert_missing(CreateChangesetParts::try_from(&m), "delta");
    }
}

#[test]
fn merge_missing_required_fields() {
    let mut r = Rng::new(5);
    let ok = v1::MergeRequest::from(&MergeParts {
        a: g::oid(&mut r),
        b: g::oid(&mut r),
        meta: meta(&mut r),
        change_id: g::change_id(&mut r),
    });
    assert!(MergeParts::try_from(&ok).is_ok());
    for (what, m) in [
        (
            "a",
            v1::MergeRequest {
                a: vec![],
                ..ok.clone()
            },
        ),
        (
            "b",
            v1::MergeRequest {
                b: vec![],
                ..ok.clone()
            },
        ),
        (
            "meta_fields",
            v1::MergeRequest {
                meta_fields: None,
                ..ok.clone()
            },
        ),
        (
            "change_id",
            v1::MergeRequest {
                change_id: vec![],
                ..ok.clone()
            },
        ),
        ("all", v1::MergeRequest::default()),
    ] {
        assert_missing(MergeParts::try_from(&m), what);
    }
    let m = v1::MergeRequest {
        a: vec![0; 34],
        ..ok
    };
    assert_eq!(
        MergeParts::try_from(&m).unwrap_err().subcode(),
        Some(subcode::BAD_ID_TEXT)
    );
}

/// Delta paths, tree entry names and non-empty extensions go through scion-object validation,
/// implemented by P01 in the same wave. Run after the wave merges:
/// `cargo test -p scion-proto --test proto_convert -- --ignored needs_p01`.
#[test]
#[ignore = "needs scion-object RepoPath/Name/Extensions (P01, same wave)"]
fn needs_p01_delta_tree_and_extension_round_trip() {
    use scion_core::Mode;
    use scion_object::{DeltaEntry, DeltaOp, Name, RepoPath, TreeEntry};
    let mut r = Rng::new(77);
    for i in 0..proptest_cases(500) {
        let path = RepoPath::parse(format!("dir{}/file{i}", r.below(5)).as_bytes()).unwrap();
        let d = DeltaEntry {
            path: path.clone(),
            op: if r.bool() {
                DeltaOp::Put {
                    mode: [Mode::File, Mode::Executable, Mode::Symlink][r.below(3) as usize],
                    id: scion_core::ObjectId::hash_blob(&r.bytes(10)),
                    size: r.below(1000),
                }
            } else {
                DeltaOp::Delete
            },
        };
        let back = DeltaEntry::try_from(&wire(&v1::DeltaEntryMsg::from(&d))).unwrap();
        assert_eq!(back, d);

        let te = TreeEntry {
            name: Name::new(format!("n{i}").as_bytes()).unwrap(),
            mode: Mode::File,
            id: scion_core::ObjectId::hash_blob(b"x"),
            size: 1,
        };
        assert_eq!(
            TreeEntry::try_from(&wire(&v1::TreeEntryMsg::from(&te))).unwrap(),
            te
        );

        let mut x = Extensions::new();
        x.insert(2, r.bytes(8)).unwrap();
        x.insert(4, r.bytes(8)).unwrap();
        let back = convert::extensions_from_proto(&convert::extensions_to_proto(&x)).unwrap();
        assert_eq!(back, x);
    }
    // A delta path violating SPEC 4.4 is invalid_path.
    let bad = v1::DeltaEntryMsg {
        path: b"/abs".to_vec(),
        op: convert::DELTA_OP_DELETE,
        ..Default::default()
    };
    assert_eq!(
        DeltaEntry::try_from(&bad).unwrap_err().subcode(),
        Some(subcode::INVALID_PATH)
    );
}
