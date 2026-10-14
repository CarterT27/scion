//! SPEC 19.12 policy semantics (P05): longest prefix, component-wise matching, independent
//! read/write resolution, nested groups, cycles, `github-id:`, case-insensitive logins, ref
//! pattern precedence, default deny, admin permissions, validation errors and the epoch.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_core::{ErrorCode, Principal, ScionError};
use scion_object::{RefName, RepoPath};
use scion_policy::{AdminPerm, Policy, PolicyError};

const SPEC_EXAMPLE: &str = r#"
version = 1

[groups]
eng = ["github:alice", "github:bob", "group:agents"]
agents = ["service:agent-pool", "service:ci"]
security = ["github:carol"]
admins = ["github:alice"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]

[refs."main"]
update = ["group:eng"]
force = []

[refs."release/*"]
update = ["group:admins"]
force = ["group:admins"]

[refs."green/*"]
update = ["service:ci"]
force = ["service:ci"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]          # create/delete refs
"#;

fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

fn gh_id(login: &str, id: u64) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: Some(id),
    }
}

fn svc(name: &str) -> Principal {
    Principal::Service(name.into())
}

fn spec() -> Policy {
    Policy::parse(SPEC_EXAMPLE).unwrap()
}

#[test]
fn spec_example_parses() {
    let p = spec();
    assert_ne!(p.epoch(), 0);
    assert!(p.can_read(&gh("alice"), &RepoPath::root()));
    assert!(p.can_write(&gh("bob"), &RepoPath::root()));
    assert!(p.can_read(&svc("ci"), &RepoPath::root()));
    assert!(!p.can_read(&gh("carol"), &RepoPath::root()));
    assert!(p.read_governed_by_root(&RepoPath::root()));
}

#[test]
fn longest_prefix_wins() {
    let p = spec();
    // Under /secrets only security may read or write; eng membership does not merge in.
    for path in [&b"secrets"[..], b"secrets/x", b"secrets/a/b/c"] {
        assert!(p.can_read_bytes(&gh("carol"), path));
        assert!(p.can_write_bytes(&gh("carol"), path));
        assert!(!p.can_read_bytes(&gh("alice"), path));
        assert!(!p.can_write_bytes(&gh("alice"), path));
        assert!(!p.read_governed_by_root_bytes(path));
    }
    // Elsewhere the root rule governs.
    for path in [&b""[..], b"src", b"src/main.rs"] {
        assert!(p.can_read_bytes(&gh("alice"), path));
        assert!(!p.can_read_bytes(&gh("carol"), path));
        assert!(p.read_governed_by_root_bytes(path));
    }

    // Deeper rules beat shallower ones regardless of declaration order.
    let p = Policy::parse(
        r#"
version = 1
[[paths]]
prefix = "/a/b/c"
read = ["github:c"]
write = ["github:c"]
[[paths]]
prefix = "/"
read = ["github:root"]
write = ["github:root"]
[[paths]]
prefix = "/a"
read = ["github:a"]
write = ["github:a"]
"#,
    )
    .unwrap();
    assert!(p.can_read_bytes(&gh("a"), b"a"));
    assert!(p.can_read_bytes(&gh("a"), b"a/b"));
    assert!(p.can_read_bytes(&gh("a"), b"a/b/x"));
    assert!(!p.can_read_bytes(&gh("a"), b"a/b/c"));
    assert!(p.can_read_bytes(&gh("c"), b"a/b/c/d"));
    assert!(!p.can_read_bytes(&gh("root"), b"a/b/c/d"));
    assert!(p.can_read_bytes(&gh("root"), b"b"));
}

#[test]
fn component_wise_matching() {
    let p = spec();
    assert!(!p.can_read_bytes(&gh("carol"), b"secrets2"));
    assert!(p.can_read_bytes(&gh("alice"), b"secrets2"));
    assert!(p.can_read_bytes(&gh("alice"), b"secrets2/x"));
    assert!(p.can_read_bytes(&gh("alice"), b"secret"));
    assert!(p.can_read_bytes(&gh("alice"), b"x/secrets"));
    assert!(p.read_governed_by_root_bytes(b"secrets2"));
    assert!(!p.read_governed_by_root_bytes(b"secrets"));
}

#[test]
fn trailing_slash_prefix_is_normalized() {
    let a = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = []\nwrite = []\n[[paths]]\nprefix = \"/docs/\"\nread = [\"github:d\"]\nwrite = []\n",
    )
    .unwrap();
    let b = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = []\nwrite = []\n[[paths]]\nprefix = \"/docs\"\nread = [\"github:d\"]\nwrite = []\n",
    )
    .unwrap();
    assert_eq!(a.epoch(), b.epoch());
    assert!(a.can_read_bytes(&gh("d"), b"docs/x"));
    assert!(!a.can_read_bytes(&gh("d"), b"docsx"));
}

#[test]
fn independent_read_and_write_resolution() {
    let p = Policy::parse(
        r#"
version = 1
[[paths]]
prefix = "/"
read = ["github:everyone", "github:writer"]
write = ["github:root-writer"]

# Only write is governed here; read still comes from "/".
[[paths]]
prefix = "/docs"
write = ["github:writer"]

# Only read is governed here; write still comes from "/".
[[paths]]
prefix = "/private"
read = ["github:reader"]
"#,
    )
    .unwrap();
    // /docs: read from "/", write from "/docs".
    assert!(p.can_read_bytes(&gh("everyone"), b"docs/a"));
    assert!(p.read_governed_by_root_bytes(b"docs/a"));
    assert!(p.can_write_bytes(&gh("writer"), b"docs/a"));
    assert!(!p.can_write_bytes(&gh("root-writer"), b"docs/a"));
    // /private: read from "/private", write from "/".
    assert!(p.can_read_bytes(&gh("reader"), b"private/a"));
    assert!(!p.can_read_bytes(&gh("everyone"), b"private/a"));
    assert!(!p.read_governed_by_root_bytes(b"private/a"));
    assert!(p.can_write_bytes(&gh("root-writer"), b"private/a"));
    assert!(!p.can_write_bytes(&gh("reader"), b"private/a"));
    // Read never implies write and vice versa.
    assert!(p.can_read_bytes(&gh("writer"), b"x"));
    assert!(!p.can_write_bytes(&gh("writer"), b"x"));
    assert!(p.can_write_bytes(&gh("root-writer"), b"x"));
    assert!(!p.can_read_bytes(&gh("root-writer"), b"x"));
}

#[test]
fn root_rule_omitting_a_key_denies() {
    let p =
        Policy::parse("version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:a\"]\n").unwrap();
    assert!(p.can_read_bytes(&gh("a"), b"x"));
    assert!(!p.can_write_bytes(&gh("a"), b"x"));
}

#[test]
fn nested_groups() {
    let p = spec();
    // agents is nested into eng.
    assert!(p.can_write_bytes(&svc("agent-pool"), b"src/x"));
    assert_eq!(p.groups_of(&svc("ci")), vec!["agents", "eng"]);
    assert_eq!(p.groups_of(&gh("alice")), vec!["admins", "eng"]);
    assert_eq!(p.groups_of(&gh("carol")), vec!["security"]);
    assert!(p.groups_of(&gh("mallory")).is_empty());

    // Three levels deep, declared out of order.
    let p = Policy::parse(
        r#"
version = 1
[groups]
top = ["group:mid"]
mid = ["group:leaf", "github:m"]
leaf = ["service:deep"]
[[paths]]
prefix = "/"
read = ["group:top"]
write = []
"#,
    )
    .unwrap();
    assert!(p.can_read_bytes(&svc("deep"), b"a"));
    assert!(p.can_read_bytes(&gh("m"), b"a"));
    assert_eq!(p.groups_of(&svc("deep")), vec!["leaf", "mid", "top"]);
}

#[test]
fn group_cycle_detection() {
    let err = Policy::parse(
        r#"
version = 1
[groups]
a = ["group:b"]
b = ["group:c"]
c = ["group:a", "github:x"]
[[paths]]
prefix = "/"
read = ["group:a"]
"#,
    )
    .unwrap_err();
    match err {
        PolicyError::GroupCycle(cycle) => {
            assert_eq!(cycle, vec!["a", "b", "c", "a"]);
        }
        other => panic!("expected GroupCycle, got {other:?}"),
    }

    let self_cycle =
        Policy::parse("version = 1\n[groups]\nme = [\"group:me\"]\n[[paths]]\nprefix = \"/\"\n")
            .unwrap_err();
    assert_eq!(
        self_cycle,
        PolicyError::GroupCycle(vec!["me".into(), "me".into()])
    );
}

#[test]
fn unknown_group_rejected() {
    let in_group =
        Policy::parse("version = 1\n[groups]\na = [\"group:nope\"]\n[[paths]]\nprefix = \"/\"\n")
            .unwrap_err();
    assert_eq!(in_group, PolicyError::UnknownGroup("nope".into()));
    let in_rule =
        Policy::parse("version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"group:ghost\"]\n")
            .unwrap_err();
    assert_eq!(in_rule, PolicyError::UnknownGroup("ghost".into()));
    let in_ref = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\n[refs.main]\nupdate = [\"group:ghost\"]\n",
    )
    .unwrap_err();
    assert_eq!(in_ref, PolicyError::UnknownGroup("ghost".into()));
}

#[test]
fn github_id_form() {
    let p = Policy::parse(
        r#"
version = 1
[groups]
ops = ["github-id:1001"]
[[paths]]
prefix = "/"
read = ["github-id:42", "group:ops"]
write = ["github-id:42"]
"#,
    )
    .unwrap();
    // Matches only when the token carries that numeric id, whatever the login.
    assert!(p.can_read_bytes(&gh_id("renamed-user", 42), b"x"));
    assert!(p.can_write_bytes(&gh_id("whoever", 42), b"x"));
    assert!(!p.can_read_bytes(&gh_id("other", 43), b"x"));
    assert!(!p.can_read_bytes(&gh("renamed-user"), b"x"));
    // A login that looks like the number does not match.
    assert!(!p.can_read_bytes(&gh("42"), b"x"));
    assert!(p.can_read_bytes(&gh_id("op", 1001), b"x"));
    assert_eq!(p.groups_of(&gh_id("op", 1001)), vec!["ops"]);
    // Login and id forms both apply to the same principal.
    let p = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:alice\"]\nwrite = [\"github-id:7\"]\n",
    )
    .unwrap();
    let alice = gh_id("Alice", 7);
    assert!(p.can_read_bytes(&alice, b"f"));
    assert!(p.can_write_bytes(&alice, b"f"));
}

#[test]
fn github_logins_case_insensitive() {
    let p = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:MixedCase\"]\nwrite = []\n",
    )
    .unwrap();
    for login in ["mixedcase", "MIXEDCASE", "MixedCase", "mIxEdCaSe"] {
        assert!(p.can_read_bytes(&gh(login), b"a"), "{login}");
    }
    assert!(!p.can_read_bytes(&gh("mixedcas"), b"a"));
    // Service names are compared exactly.
    let p = Policy::parse(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"service:CI\"]\nwrite = []\n",
    )
    .unwrap();
    assert!(p.can_read_bytes(&svc("CI"), b"a"));
    assert!(!p.can_read_bytes(&svc("ci"), b"a"));
    // Differently-cased logins in the file are equivalent policies.
    let a =
        Policy::parse("version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:BOB\"]\n").unwrap();
    let b =
        Policy::parse("version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:bob\"]\n").unwrap();
    assert_eq!(a.epoch(), b.epoch());
}

#[test]
fn ref_pattern_precedence_and_default_deny() {
    let p = Policy::parse(
        r#"
version = 1
[[paths]]
prefix = "/"
read = []
write = []

[refs."*"]
update = ["github:anyone"]

[refs."release/*"]
update = ["github:rel"]
force = ["github:rel"]

[refs."release/v1/*"]
update = ["github:v1"]

[refs."release/v1/hotfix"]
update = ["github:exact"]

[refs."main"]
update = ["github:dev"]
force = []
"#,
    )
    .unwrap();
    let names: &[(&str, &str)] = &[
        // exact beats every pattern
        ("release/v1/hotfix", "exact"),
        ("main", "dev"),
        // longest pattern among patterns
        ("release/v1/x", "v1"),
        ("release/v1/x/y", "v1"),
        ("release/v2", "rel"),
        ("release/a/b", "rel"),
        // only "*" matches
        ("feature/x", "anyone"),
        ("release", "anyone"),
        ("release/v1", "rel"),
    ];
    let everyone = ["exact", "dev", "v1", "rel", "anyone"];
    for (name, winner) in names {
        for who in everyone {
            assert_eq!(
                p.can_update_ref_name(&gh(who), name),
                who == *winner,
                "{who} on {name}"
            );
        }
    }
    // force is resolved by the same rule independently of update.
    assert!(p.can_force_ref_name(&gh("rel"), "release/v2"));
    assert!(!p.can_force_ref_name(&gh("v1"), "release/v1/x"));
    assert!(!p.can_force_ref_name(&gh("rel"), "release/v1/x"));
    assert!(!p.can_force_ref_name(&gh("dev"), "main"));
    assert!(p.can_update_ref(&gh("dev"), &RefName::main()));
    assert!(!p.can_force_ref(&gh("dev"), &RefName::main()));

    // Default deny: no matching rule at all.
    let spec = spec();
    assert!(!spec.can_update_ref_name(&gh("alice"), "feature/x"));
    assert!(!spec.can_force_ref_name(&gh("alice"), "feature/x"));
    assert!(spec.can_update_ref_name(&gh("alice"), "release/1.0"));
    assert!(!spec.can_update_ref_name(&gh("bob"), "release/1.0"));
    assert!(spec.can_update_ref_name(&svc("ci"), "green/abc"));
    assert!(!spec.can_update_ref_name(&svc("ci"), "green"));
    assert!(spec.can_update_ref(&gh("bob"), &RefName::main()));
    assert!(!spec.can_force_ref(&gh("alice"), &RefName::main()));
    // A principal in no list is denied everything.
    let nobody = gh("mallory");
    assert!(!spec.can_read_bytes(&nobody, b""));
    assert!(!spec.can_write_bytes(&nobody, b"x"));
    assert!(!spec.can_update_ref(&nobody, &RefName::main()));
    for a in [AdminPerm::Redact, AdminPerm::Purge, AdminPerm::Refs] {
        assert!(!spec.can_admin(&nobody, a));
    }
}

#[test]
fn admin_permissions() {
    let p = spec();
    for a in [AdminPerm::Redact, AdminPerm::Purge, AdminPerm::Refs] {
        assert!(p.can_admin(&gh("alice"), a));
        assert!(p.can_admin(&gh("ALICE"), a));
        assert!(!p.can_admin(&gh("bob"), a));
        assert!(!p.can_admin(&svc("ci"), a));
    }
    let p = Policy::parse(
        r#"
version = 1
[[paths]]
prefix = "/"
[admin]
redact = ["github:r"]
purge = ["github:p"]
"#,
    )
    .unwrap();
    assert!(p.can_admin(&gh("r"), AdminPerm::Redact));
    assert!(!p.can_admin(&gh("r"), AdminPerm::Purge));
    assert!(p.can_admin(&gh("p"), AdminPerm::Purge));
    assert!(!p.can_admin(&gh("p"), AdminPerm::Redact));
    assert!(!p.can_admin(&gh("r"), AdminPerm::Refs));
    assert!(!p.can_admin(&gh("p"), AdminPerm::Refs));
}

#[test]
fn allow_all_for_tests_grants_everything() {
    let p = Policy::allow_all_for_tests();
    for who in [gh("x"), gh_id("y", 3), svc("z")] {
        assert!(p.can_read(&who, &RepoPath::root()));
        assert!(p.can_write(&who, &RepoPath::root()));
        assert!(p.can_read_bytes(&who, b"secrets/deep"));
        assert!(p.can_write_bytes(&who, b"secrets/deep"));
        assert!(p.can_update_ref(&who, &RefName::main()));
        assert!(p.can_force_ref_name(&who, "release/1"));
        for a in [AdminPerm::Redact, AdminPerm::Purge, AdminPerm::Refs] {
            assert!(p.can_admin(&who, a));
        }
    }
    assert!(p.read_governed_by_root_bytes(b"any/path"));
    assert_eq!(p.epoch(), Policy::allow_all_for_tests().epoch());
    assert_ne!(p.epoch(), spec().epoch());
}

#[test]
fn validation_errors() {
    let cases: &[(&str, PolicyError)] = &[
        (
            "version = 2\n[[paths]]\nprefix = \"/\"\n",
            PolicyError::UnsupportedVersion(2),
        ),
        ("version = 1\n", PolicyError::MissingRootRule),
        (
            "version = 1\n[[paths]]\nprefix = \"/a\"\nread = []\n",
            PolicyError::MissingRootRule,
        ),
        (
            "version = 1\nextra = 3\n[[paths]]\nprefix = \"/\"\n",
            PolicyError::UnknownKey("extra".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\nreed = []\n",
            PolicyError::UnknownKey("paths[0].reed".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[refs.main]\nupdat = []\n",
            PolicyError::UnknownKey("refs.main.updat".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[admin]\ndelete = []\n",
            PolicyError::UnknownKey("admin.delete".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"alice\"]\n",
            PolicyError::BadPrincipal("alice".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github-id:abc\"]\n",
            PolicyError::BadPrincipal("github-id:abc".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"secrets\"\n",
            PolicyError::BadPrefix("secrets".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[[paths]]\nprefix = \"/a/../b\"\n",
            PolicyError::BadPrefix("/a/../b".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[refs.\"rel*\"]\n",
            PolicyError::BadRefPattern("rel*".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[refs.\"a/*/b\"]\n",
            PolicyError::BadRefPattern("a/*/b".into()),
        ),
        (
            "version = 1\n[[paths]]\nprefix = \"/\"\n[refs.\"Main\"]\n",
            PolicyError::BadRefPattern("Main".into()),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(&Policy::parse(text).unwrap_err(), want, "{text}");
    }
    // Duplicate prefix after normalization.
    assert!(matches!(
        Policy::parse(
            "version = 1\n[[paths]]\nprefix = \"/\"\n[[paths]]\nprefix = \"/a\"\n[[paths]]\nprefix = \"/a/\"\n"
        ),
        Err(PolicyError::BadPrefix(_))
    ));
    // Missing or mistyped version, syntax errors, wrong types.
    for text in [
        "[[paths]]\nprefix = \"/\"\n",
        "version = \"1\"\n[[paths]]\nprefix = \"/\"\n",
        "version = 1\n[[paths]\n",
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = \"github:a\"\n",
        "version = 1\npaths = 3\n",
    ] {
        assert!(
            matches!(Policy::parse(text), Err(PolicyError::Toml(_))),
            "{text}: {:?}",
            Policy::parse(text)
        );
    }
}

#[test]
fn policy_error_maps_to_invalid_argument() {
    let e: ScionError = PolicyError::MissingRootRule.into();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.message.contains('/'));
    let e: ScionError = PolicyError::UnknownKey("paths[0].reed".into()).into();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.message.contains("paths[0].reed"));
}

#[test]
fn equivalent_files_share_an_epoch() {
    let reordered = r#"
# Same policy as SPEC_EXAMPLE: different order, spacing, casing and duplicates.
version = 1
[admin]
refs = ["group:admins"]
purge = ["group:admins", "group:admins"]
redact = ["group:admins"]

[[paths]]
prefix = "/secrets/"
write = ["group:security"]
read = ["group:security"]

[[paths]]
prefix = "/"
write = ["group:eng"]
read = ["group:eng"]

[refs."green/*"]
force = ["service:ci"]
update = ["service:ci"]

[refs."release/*"]
force = ["group:admins"]
update = ["group:admins"]

[refs.main]
update = ["group:eng"]

[groups]
admins = ["github:ALICE"]
security = ["github:Carol"]
agents = ["service:ci", "service:agent-pool"]
eng = ["group:agents", "github:bob", "github:alice"]
"#;
    assert_eq!(Policy::parse(reordered).unwrap().epoch(), spec().epoch());

    // Any semantic change moves the epoch.
    let changed = SPEC_EXAMPLE.replace(
        "security = [\"github:carol\"]",
        "security = [\"github:dave\"]",
    );
    assert_ne!(Policy::parse(&changed).unwrap().epoch(), spec().epoch());
    let changed = SPEC_EXAMPLE.replace("prefix = \"/secrets\"", "prefix = \"/secret\"");
    assert_ne!(Policy::parse(&changed).unwrap().epoch(), spec().epoch());
}

/// The frozen `RepoPath`/`RefName` entry points delegate to the byte/str forms. Non-root
/// `RepoPath`s are built with `RepoPath::parse` (P01); skip while that is still a stub.
#[test]
fn frozen_entry_points_with_parsed_paths() {
    let parsed = std::panic::catch_unwind(|| RepoPath::parse(b"secrets/x"));
    let Ok(Ok(path)) = parsed else {
        eprintln!("SKIP: RepoPath::parse unavailable (P01 not merged)");
        return;
    };
    let p = spec();
    assert!(p.can_read(&gh("carol"), &path));
    assert!(!p.can_read(&gh("alice"), &path));
    assert!(!p.read_governed_by_root(&path));
    let other = RepoPath::parse(b"secrets2").unwrap();
    assert!(p.can_write(&gh("alice"), &other));
    assert!(p.read_governed_by_root(&other));
    if let Ok(Ok(r)) = std::panic::catch_unwind(|| RefName::parse("release/1.0")) {
        assert!(p.can_update_ref(&gh("alice"), &r));
        assert!(p.can_force_ref(&gh("alice"), &r));
        assert!(!p.can_update_ref(&gh("bob"), &r));
    }
}
