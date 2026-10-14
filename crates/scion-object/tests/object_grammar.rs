//! SPEC 4.4 path rules (19.2 violations at parse level), SPEC 4.3.6 ref-name grammar and the
//! SPEC 11.8 revision grammar (PLAN 6.6).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::{ErrorCode, Kind, Limits, Mode, ObjectId, subcode};
use scion_object::{
    Changeset, DeltaEntry, DeltaOp, Extensions, Name, RefName, RepoPath, RevBase, RevSuffix,
    Revision, parse_revision,
};

fn path_err(b: &[u8]) {
    let e = RepoPath::parse(b).expect_err(&format!(
        "{:?} should be rejected",
        String::from_utf8_lossy(b)
    ));
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some(subcode::INVALID_PATH));
}

#[test]
fn path_rule_violations() {
    let n255 = vec![b'n'; 255];
    let n256 = vec![b'n'; 256];
    let mut cases: Vec<Vec<u8>> = vec![
        b".".to_vec(),
        b"..".to_vec(),
        b"a/.".to_vec(),
        b"a/../b".to_vec(),
        b"/".to_vec(),
        b"/a".to_vec(),
        b"a/".to_vec(),
        b"a//b".to_vec(),
        b"a\0b".to_vec(),
        b"\0".to_vec(),
        b".scion".to_vec(),
        b".SCION".to_vec(),
        b".Scion".to_vec(),
        b"x/.sCiOn/y".to_vec(),
        n256.clone(),
        [b"a/".as_slice(), &n256].concat(),
    ];
    // 4097-byte path made of valid components.
    let mut long = Vec::new();
    while long.len() < 4097 {
        if !long.is_empty() {
            long.push(b'/');
        }
        long.extend_from_slice(&n255);
    }
    long.truncate(4097);
    assert_eq!(long.len(), 4097);
    assert_ne!(long.last(), Some(&b'/'));
    cases.push(long.clone());
    for c in &cases {
        path_err(c);
    }

    // Name-level violations.
    for bad in [
        &b"."[..],
        b"..",
        b"/",
        b"a/b",
        b"\0",
        b".scion",
        b".SCION",
        b".Scion",
        &n256,
        b"",
    ] {
        assert_eq!(
            Name::new(bad).unwrap_err().subcode(),
            Some(subcode::INVALID_PATH)
        );
    }

    // Boundaries that are valid.
    // 40 components of 100 bytes (4039 bytes) plus "/" and 56 bytes = 4096.
    let mut ok4096 = vec![vec![b'n'; 100]; 40].join(&b'/');
    ok4096.push(b'/');
    ok4096.extend_from_slice(&[b'm'; 56]);
    assert_eq!(ok4096.len(), 4096);
    assert!(RepoPath::parse(&ok4096).is_ok());
    assert!(RepoPath::parse(&n255).is_ok());
    assert!(RepoPath::parse(b"").unwrap().is_root());
    for good in [
        &b"..."[..],
        b".scion2",
        b"a.scion",
        b"x/.git/y",
        b"\xff\xfe",
        b"A/b c/d",
    ] {
        assert!(RepoPath::parse(good).is_ok(), "{good:?}");
    }
}

#[test]
fn delta_order_errors_do_not_echo_raw_paths() {
    // Implementation-defined 6.9: raw repository paths never reach logs, so error text
    // carries entry indices instead.
    let del = |p: &str| DeltaEntry {
        path: RepoPath::parse(p.as_bytes()).unwrap(),
        op: DeltaOp::Delete,
    };
    for (delta, want) in [
        (
            vec![del("secret/zz"), del("secret/aa")],
            subcode::DELTA_UNSORTED,
        ),
        (
            vec![del("secret/aa"), del("secret/aa")],
            subcode::DELTA_DUPLICATE,
        ),
    ] {
        let cs = Changeset {
            parents: vec![],
            root_tree: scion_core::empty_tree_id(),
            meta: ObjectId::new(Kind::Meta, [0; 32]),
            change_id: scion_core::ChangeId([0; 16]),
            delta,
            extensions: Extensions::new(),
        };
        let e = cs.validate_shape(&Limits::default()).unwrap_err();
        assert_eq!(e.subcode(), Some(want));
        assert!(!e.message.contains("secret"), "{}", e.message);
    }
}

#[test]
fn delta_path_violations_fail_validate_shape() {
    // Valid RepoPaths are guaranteed by construction; the root is the only bad delta path.
    let cs = Changeset {
        parents: vec![],
        root_tree: scion_core::empty_tree_id(),
        meta: ObjectId::new(Kind::Meta, [0; 32]),
        change_id: scion_core::ChangeId([0; 16]),
        delta: vec![DeltaEntry {
            path: RepoPath::root(),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: ObjectId::hash_blob(b""),
                size: 0,
            },
        }],
        extensions: Extensions::new(),
    };
    assert_eq!(
        cs.validate_shape(&Limits::default()).unwrap_err().subcode(),
        Some(subcode::INVALID_PATH)
    );
}

#[test]
fn ref_name_grammar_table() {
    let long_ok = format!("a{}", "b".repeat(199));
    let long_bad = format!("a{}", "b".repeat(200));
    let valid: &[&str] = &[
        "main",
        "release/1.0",
        "green/ci-linux_x86",
        "0",
        "a",
        "9lives",
        "a.b",
        "a-b_c.d/e",
        "release/2024.10/rc-1",
        "x/y/z",
        "a.",
        "a_",
        &long_ok,
    ];
    for s in valid {
        let r = RefName::parse(s).unwrap_or_else(|e| panic!("{s:?} should be valid: {e}"));
        assert_eq!(r.as_str(), *s);
        assert_eq!(s.parse::<RefName>().unwrap(), r);
    }
    let invalid: &[&str] = &[
        "",
        "Main",
        "MAIN",
        "-main",
        ".main",
        "_main",
        "/main",
        "release/",
        "a//b",
        "a..b",
        "..",
        "a/../b",
        "main branch",
        "main@1",
        "main~1",
        "main^",
        "feat:x",
        "caf\u{e9}",
        "a\0",
        "a\\b",
        "tab\t",
        &long_bad,
    ];
    for s in invalid {
        let e = RefName::parse(s).expect_err(&format!("{s:?} should be invalid"));
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::INVALID_REF_NAME), "{s:?}");
    }
    assert_eq!(RefName::main().as_str(), "main");
}

fn rev(base: RevBase, suffixes: Vec<RevSuffix>) -> Revision {
    Revision { base, suffixes }
}

fn name(s: &str) -> RevBase {
    RevBase::Name(s.to_owned())
}

#[test]
fn revision_grammar_table() {
    let cs = ObjectId::new(Kind::Changeset, [0xab; 32]);
    let tree = ObjectId::new(Kind::Tree, [0x01; 32]);
    let node = ObjectId::new(Kind::Derived(0x10), [0x02; 32]);
    let cs_text = cs.to_text();
    let sha = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let hex63 = "a".repeat(63);
    let main = RefName::main();
    use RevSuffix::{Ancestor, Parent};

    let valid: Vec<(String, Revision)> = vec![
        (cs_text.clone(), rev(RevBase::Id(cs), vec![])),
        (tree.to_text(), rev(RevBase::Id(tree), vec![])),
        (node.to_text(), rev(RevBase::Id(node), vec![])),
        (
            format!("{cs_text}~2"),
            rev(RevBase::Id(cs), vec![Ancestor(2)]),
        ),
        (
            "c1abcdef01".into(),
            rev(RevBase::IdPrefix("c1abcdef01".into()), vec![]),
        ),
        (
            "c112345678".into(),
            rev(RevBase::IdPrefix("c112345678".into()), vec![]),
        ),
        (
            format!("c1{hex63}"),
            rev(RevBase::IdPrefix(format!("c1{hex63}")), vec![]),
        ),
        (
            "c1abcdef01^2".into(),
            rev(RevBase::IdPrefix("c1abcdef01".into()), vec![Parent(2)]),
        ),
        ("main".into(), rev(name("main"), vec![])),
        ("release/1.0".into(), rev(name("release/1.0"), vec![])),
        ("deadbeef".into(), rev(name("deadbeef"), vec![])),
        (
            "0123456789abcdef".into(),
            rev(name("0123456789abcdef"), vec![]),
        ),
        // Shorter than 8 hex: still a valid ref name; the server decides.
        ("abc".into(), rev(name("abc"), vec![])),
        // "c1" + 7 hex is not an IdPrefix, but it is a valid ref name.
        ("c1abcdef0".into(), rev(name("c1abcdef0"), vec![])),
        (
            "main@1".into(),
            rev(RevBase::RefAt(main.clone(), 1), vec![]),
        ),
        (
            "main@0".into(),
            rev(RevBase::RefAt(main.clone(), 0), vec![]),
        ),
        (
            "release/1.0@18446744073709551615".into(),
            rev(
                RevBase::RefAt(RefName::parse("release/1.0").unwrap(), u64::MAX),
                vec![],
            ),
        ),
        (
            "main@3~1".into(),
            rev(RevBase::RefAt(main.clone(), 3), vec![Ancestor(1)]),
        ),
        (format!("git:{sha}"), rev(RevBase::Git(sha.into()), vec![])),
        (
            format!("git:{sha}^"),
            rev(RevBase::Git(sha.into()), vec![Parent(1)]),
        ),
        ("main~".into(), rev(name("main"), vec![Ancestor(1)])),
        ("main^".into(), rev(name("main"), vec![Parent(1)])),
        ("main~0".into(), rev(name("main"), vec![Ancestor(0)])),
        ("main~3".into(), rev(name("main"), vec![Ancestor(3)])),
        ("main^2".into(), rev(name("main"), vec![Parent(2)])),
        (
            "main~2^2~".into(),
            rev(name("main"), vec![Ancestor(2), Parent(2), Ancestor(1)]),
        ),
        (
            "main^^".into(),
            rev(name("main"), vec![Parent(1), Parent(1)]),
        ),
        (
            "main~~3".into(),
            rev(name("main"), vec![Ancestor(1), Ancestor(3)]),
        ),
        (
            "main~4294967295".into(),
            rev(name("main"), vec![Ancestor(u32::MAX)]),
        ),
        ("main~010".into(), rev(name("main"), vec![Ancestor(10)])),
    ];
    for (s, want) in &valid {
        let got = parse_revision(s).unwrap_or_else(|e| panic!("{s:?} should parse: {e}"));
        assert_eq!(&got, want, "{s:?}");
    }

    let invalid: Vec<String> = vec![
        "".into(),
        "~1".into(),
        "^".into(),
        "main^0".into(),
        "main~^0".into(),
        "main~-1".into(),
        "main~x".into(),
        "main~1x".into(),
        "main~4294967296".into(),
        "main^99999999999".into(),
        "Main".into(),
        "MAIN~1".into(),
        "main@".into(),
        "@1".into(),
        "main@x".into(),
        "main@-1".into(),
        "main@1@2".into(),
        "main@18446744073709551616".into(),
        "Bad@1".into(),
        "git:".into(),
        "git:abc".into(),
        format!("git:{}", "A".repeat(40)),
        format!("git:{sha}0"),
        cs_text.to_uppercase(),
        "a//b".into(),
        "release/".into(),
        "main branch".into(),
        "-x".into(),
        "main ~1".into(),
        // Non-ASCII after a suffix or as the whole token: rejected without panicking.
        "main~1\u{e9}".into(),
        "main^\u{e9}".into(),
        "\u{e9}".into(),
        "c1abcdef01\u{e9}".into(),
    ];
    for s in &invalid {
        let e = parse_revision(s).expect_err(&format!("{s:?} should be invalid"));
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{s:?}");
        assert_eq!(e.subcode(), Some(subcode::INVALID_REVISION), "{s:?}");
    }
    assert!(valid.len() + invalid.len() >= 25);
}

#[test]
fn hex_prefix_helper() {
    let p = |s: &str| parse_revision(s).unwrap().base;
    assert_eq!(p("c1abcdef01").hex_prefix(), Some("abcdef01"));
    assert_eq!(p("deadbeef").hex_prefix(), Some("deadbeef"));
    assert_eq!(p("c1deadbeef").hex_prefix(), Some("deadbeef"));
    assert_eq!(p("deadbee").hex_prefix(), None);
    assert_eq!(p("main").hex_prefix(), None);
    assert_eq!(p("main@2").hex_prefix(), None);
}
